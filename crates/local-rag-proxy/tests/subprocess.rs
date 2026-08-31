//! T15-02 acceptance tests driving the **real** `local-rag-proxy` binary
//! (and the real `local-rag serve` daemon it spawns) as genuine OS
//! processes — the scenarios that only exist across a real process
//! boundary: cold-start spawn, signal detachment, and end-to-end context
//! passthrough. Mirrors `crates/local-rag/tests/serve_subprocess.rs`'s own
//! established idiom (`TempHome::command`, `libc::kill` for a real SIGTERM
//! — `Child::kill` is `SIGKILL`-only on unix).

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::time::{Duration, Instant};

use local_rag_core::paths::StoreLayout;
use local_rag_core::process::pid_exists;
use local_rag_protocol::{
    MCP_PASSTHROUGH_VERSION, Message, ShutdownRequest, Welcome, decode_message, encode_message,
};
use local_rag_test_support::TempHome;

/// Locate the real `local-rag` binary next to this integration test binary.
///
/// `env!("CARGO_BIN_EXE_local-rag")` is **not** set for another package's
/// binary just because it is a dev-dependency (verified empirically — same
/// finding `local-rag-hook/tests/recall_rpc.rs::local_rag_binary_path`'s own
/// doc comment already recorded for the identical `local-rag = {path}`
/// dev-dependency pattern). Instead, mirror
/// `local-rag-proxy::connect::resolve_daemon_binary_path`'s own trick — a
/// cargo test binary lives at `target/<profile>/deps/<name>-<hash>`, and a
/// regular binary target lives one directory up, at
/// `target/<profile>/<name>` — the same "ships side by side" layout spec 13
/// §1 describes for the real npm-packaged distribution.
///
/// Only used by the `failpoints`-gated cross-binary-version upgrade test
/// below — `#[cfg]`-gated the same way so a plain `cargo test -p
/// local-rag-proxy` (no `--features failpoints`) does not trip `dead_code`.
#[cfg(feature = "failpoints")]
fn local_rag_binary_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let deps_dir = exe.parent().expect("deps dir");
    let profile_dir = deps_dir.parent().expect("profile dir");
    let candidate = profile_dir.join("local-rag");
    assert!(
        candidate.is_file(),
        "expected a sibling local-rag binary at {candidate:?} (built via this package's own \
         local-rag dev-dependency)"
    );
    candidate
}

fn open_layout(home: &TempHome) -> StoreLayout {
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    layout
}

fn spawn_proxy(home: &TempHome, extra_env: &[(&str, &str)]) -> Child {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag-proxy"));
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn().expect("spawn local-rag-proxy")
}

/// Poll `store.lock` until it parses with `ready: true`, or panic after
/// `timeout` — real, bounded wall-clock waiting is inherent to driving a
/// real child process (here, the daemon `local-rag-proxy` itself spawns)
/// across a real OS scheduling boundary. Nothing asserts on the duration,
/// only on eventual readiness.
fn wait_until_daemon_ready(layout: &StoreLayout, timeout: Duration) -> serde_json::Value {
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

/// Send a real `SIGKILL` — an ungraceful crash, unlike [`send_sigterm`]: the
/// killed process never runs its own orderly-shutdown socket cleanup (spec
/// 02 §4.3), leaving `run/daemon.sock` behind as a stale, unbound file
/// (§4.1's own as-built note: "the socket file itself has no auto-cleanup
/// and can genuinely outlive a SIGKILLed daemon") — the real-world "daemon
/// down" precondition the restart test below needs.
fn send_sigkill(pid: u32) {
    // SAFETY: same as `send_sigterm`.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    assert_eq!(
        rc,
        0,
        "kill(SIGKILL) failed: {}",
        std::io::Error::last_os_error()
    );
}

fn daemon_pid(layout: &StoreLayout) -> u32 {
    let bytes = std::fs::read(layout.store_lock()).expect("read store.lock");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("parse store.lock");
    json["pid"].as_u64().expect("pid field") as u32
}

fn write_line(stdin: &mut ChildStdin, line: &str) {
    writeln!(stdin, "{line}").expect("write stdin line");
    stdin.flush().expect("flush stdin");
}

/// Read one line, bounded by `timeout`. Takes and returns ownership of the
/// reader (rather than `&mut`) so the blocking read can run on a dedicated
/// thread: a hung proxy must fail the test with a clear timeout, not hang
/// the whole suite waiting on a synchronous read with no bound.
/// What the proxy has to say for itself, for a failure message that would
/// otherwise name a symptom and hide the mechanism (D-115).
///
/// `spawn_proxy` pipes stderr and nothing ever reads it, so when this file's
/// tests fail in CI the one artefact that would explain why is discarded. The
/// stderr is only drained once the caller already knows the process is going
/// (an EOF on stdout, or a blown timeout), so this cannot block a healthy run.
fn proxy_diagnosis(proxy: &mut Child) -> String {
    use std::io::Read as _;
    let status = match proxy.try_wait() {
        Ok(Some(status)) => format!("{status}"),
        Ok(None) => "still running".to_string(),
        Err(e) => format!("try_wait failed: {e}"),
    };
    let stderr = match proxy.stderr.take() {
        Some(mut handle) => {
            let mut buf = String::new();
            let _ = handle.read_to_string(&mut buf);
            buf
        }
        None => "<already taken>".to_string(),
    };
    let stderr = if stderr.trim().is_empty() {
        "    <empty>".to_string()
    } else {
        stderr
            .lines()
            .map(|l| format!("    {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!("\n  proxy exit: {status}\n  proxy stderr:\n{stderr}")
}

fn read_line(
    proxy: &mut Child,
    mut stdout: BufReader<ChildStdout>,
    timeout: Duration,
) -> (BufReader<ChildStdout>, String) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = stdout.read_line(&mut line).map(|read| (read, line));
        let _ = tx.send((stdout, result));
    });
    let (stdout, result) = match rx.recv_timeout(timeout) {
        Ok(pair) => pair,
        Err(_) => panic!(
            "proxy did not answer within {timeout:?}{}",
            proxy_diagnosis(proxy)
        ),
    };
    let (read, line) = result.expect("read stdout line");
    // D-115: zero bytes is EOF, not an answer. Left unchecked it reached the
    // caller as an empty string and surfaced as `parse response: EOF while
    // parsing a value` — a message that names the symptom and says nothing
    // about the proxy having exited before it answered, which is the only way
    // to get here. CI produced exactly that once, in 0.016 s, on the upgrade
    // flow, and the run carried no evidence of why.
    assert!(
        read > 0,
        "the proxy closed stdout without answering{}",
        proxy_diagnosis(proxy)
    );
    (stdout, line)
}

/// Shut a daemon down and wait for it to actually exit — test cleanup, not
/// part of the scenario under test. Called only once the daemon is already
/// known ready, so `daemon_pid` is expected to succeed.
fn cleanup_daemon(layout: &StoreLayout, timeout: Duration) {
    let pid = daemon_pid(layout);
    send_sigterm(pid);
    let deadline = Instant::now() + timeout;
    while pid_exists(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Real cold-start spawn (no daemon pre-running) through a real MCP
/// handshake, end to end across both binaries.
///
/// Before T15-03's real `RequestHandler` replaced `EchoRequestHandler`,
/// this test drove one echo call and asserted the echoed `context` matched
/// what the proxy sent — that assertion has no MCP-visible equivalent now
/// (a real handler answers with tool content, not its context back). That
/// coverage moved to `crates/local-rag/tests/mcp_tools.rs`'s
/// `explicit_context_routing_across_two_requests_on_one_connection`, and
/// transport-level context isolation stays covered by T15-02's own
/// `daemon::handshake::tests::two_requests_on_one_connection_keep_their_
/// own_context` (still run against `EchoRequestHandler`, unchanged).
#[test]
fn cold_start_spawns_a_daemon_and_completes_a_real_mcp_handshake() {
    let home = TempHome::new().expect("temp home");
    let layout = open_layout(&home);

    let mut proxy = spawn_proxy(
        &home,
        &[("LOCAL_RAG_SESSION_ID", "test-session-cold-start")],
    );
    let mut stdin = proxy.stdin.take().expect("proxy stdin");
    let stdout = BufReader::new(proxy.stdout.take().expect("proxy stdout"));

    // Real MCP handshake, cold-start spawn included:
    // initialize -> notifications/initialized -> tools/list, asserting
    // that the *next line read back* is tools/list's own response — the
    // only formulation that actually proves the notification produced no
    // stray line all the way through both binaries (T15-03's own
    // `daemon::handshake` unit test proves the daemon-local half; this is
    // the two-binary version).
    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    let (stdout, line) = read_line(&mut proxy, stdout, Duration::from_secs(20));
    let response: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse response");
    assert_eq!(response["result"]["serverInfo"]["name"], "local-rag");
    assert!(
        response["result"]["instructions"]
            .as_str()
            .expect("instructions is a string")
            .contains("search_code"),
        "{response}"
    );

    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );
    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let (stdout, line) = read_line(&mut proxy, stdout, Duration::from_secs(20));
    let response: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse response");
    assert_eq!(response["id"], serde_json::json!(2));
    let names: Vec<&str> = response["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "search_code",
            "get_file_context",
            "project_overview",
            "recall",
            "list_memory",
            "list_memory_candidates",
            "inspect_memory_evidence",
            "stats",
            "health",
            "remember",
            "approve_memory_candidate",
            "reject_memory_candidate",
            "edit_memory_candidate",
            "edit_memory",
            "retract_memory",
            "confirm_memory",
            "reject_memory",
            "merge_memories",
            "give_feedback",
        ]
    );
    // X-003: every advertised tool carries annotations, and destructiveHint
    // is true for exactly the two entry-terminating tools (retract_memory
    // and, since D-079, reject_memory) -- the acceptance
    // check this task card names explicitly (a real `local-rag-proxy` +
    // `local-rag serve` round trip, not just the in-process daemon test).
    let destructive: Vec<&str> = response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| {
            assert!(
                t["annotations"].is_object(),
                "{} has no annotations",
                t["name"]
            );
            (t["annotations"]["destructiveHint"] == serde_json::json!(true))
                .then(|| t["name"].as_str().unwrap())
        })
        .collect();
    assert_eq!(destructive, ["retract_memory", "reject_memory"]);
    let _ = stdout;

    wait_until_daemon_ready(&layout, Duration::from_secs(5)); // must already be true by now

    drop(stdin); // close stdin: the proxy's relay loop must exit cleanly
    let status = wait_for_exit(&mut proxy, Duration::from_secs(20));
    assert!(
        status.success(),
        "proxy must exit 0 once stdin closes: {status:?}"
    );

    cleanup_daemon(&layout, Duration::from_secs(20));
}

#[test]
fn sigterm_to_the_proxy_does_not_reach_the_daemon_it_spawned() {
    let home = TempHome::new().expect("temp home");
    let layout = open_layout(&home);

    let mut proxy = spawn_proxy(&home, &[("LOCAL_RAG_SESSION_ID", "test-session-detach")]);
    let mut stdin = proxy.stdin.take().expect("proxy stdin");
    let stdout = BufReader::new(proxy.stdout.take().expect("proxy stdout"));

    // Drive one real MCP call through so the handshake (and therefore the
    // spawn) is provably complete, not merely "the child process exists".
    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    let _ = read_line(&mut proxy, stdout, Duration::from_secs(20));

    wait_until_daemon_ready(&layout, Duration::from_secs(5));
    let daemon = daemon_pid(&layout);
    assert!(pid_exists(daemon), "the spawned daemon must be alive");

    send_sigterm(proxy.id());
    let status = wait_for_exit(&mut proxy, Duration::from_secs(20));
    assert!(
        status.success(),
        "the proxy must exit 0 on SIGTERM: {status:?}"
    );

    // The critical assertion: detachment via `process_group(0)` means a
    // signal sent only to the proxy's own pid never reaches the daemon it
    // spawned, which is running in its own process group.
    assert!(
        pid_exists(daemon),
        "the daemon must remain alive after the proxy that spawned it is killed"
    );

    cleanup_daemon(&layout, Duration::from_secs(20));
}

// ---------------------------------------------------------------------
// G15/D-026: the upgrade flow (spec 13 §4) end to end, and a real
// "daemon down/restart" recovery — the two scenarios the gate's own text
// names ("daemon down/restart/upgrade") that no existing test drove
// through the real `establish_session` state machine.
// ---------------------------------------------------------------------

/// Mirrors `local-rag-proxy/src/handshake.rs::MAX_UPGRADE_ROUNDS` (not
/// importable here — that crate has no `lib.rs`, only a `main.rs`, so this
/// integration test binary sees none of its internals, only the compiled
/// binary). `establish_session`'s own loop sends `SHUTDOWN_REQUEST` after
/// every round except the globally final one.
const MAX_UPGRADE_ROUNDS: u32 = 2;

/// A minimal, blocking fake "old daemon": binds `socket_path` and answers
/// `connections` consecutive connections, each with a `Welcome` whose
/// `daemon_version` deliberately does not match this workspace's real
/// build — driving the real proxy's `establish_session` through the
/// version-mismatch branch of spec 13 §4's upgrade flow. On every
/// connection whose round number is below [`MAX_UPGRADE_ROUNDS`], the real
/// loop sends a `SHUTDOWN_REQUEST` before waiting for this fake to close —
/// this reads and discards that line to let `wait_for_close` succeed; the
/// globally final round gets no such message (the real loop just gives up
/// there), so nothing further is read on it. Dropping the listener once
/// `connections` have been served un-binds the socket, so a next connect
/// attempt genuinely fails (nothing listening) rather than queuing.
fn spawn_fake_mismatched_daemon(
    socket_path: std::path::PathBuf,
    connections: u32,
) -> std::thread::JoinHandle<()> {
    // D-111: bind on the CALLER's thread, not inside the spawned one. Every
    // caller does `spawn_fake_*(...)` and then immediately `spawn_proxy(...)`,
    // so a bind that happens inside the thread is racing the proxy's first
    // connect. Lose that race and the proxy finds no socket, does exactly what
    // it is supposed to do — spawns a REAL `local-rag serve` — and the test
    // then asserts against a real daemon's answer while the fake sits waiting
    // for a connection that never comes. It surfaced as a real daemon's
    // `initialize` result arriving where an error was required.
    //
    // The race was invisible for as long as `syspolicyd` charged 20-45 s for
    // the first exec of a freshly built binary: the thread always won by four
    // orders of magnitude. Removing that tax made the machine fast enough for
    // the proxy to win. Binding here removes the race rather than widening a
    // window: the socket exists before this function returns, so the caller's
    // `spawn_proxy` cannot precede it.
    let listener = UnixListener::bind(&socket_path).expect("bind fake daemon socket");
    std::thread::spawn(move || {
        for round in 1..=connections {
            let (stream, _) = listener.accept().expect("accept fake daemon connection");
            let mut reader = BufReader::new(stream.try_clone().expect("clone fake stream"));
            let mut writer = stream;

            let mut hello_line = String::new();
            reader.read_line(&mut hello_line).expect("read HELLO");

            let welcome = Message::Welcome(Welcome {
                proto: local_rag_protocol::PROTO_VERSION,
                daemon_version: "0.0.0-fake-old".to_string(),
                store_instance_uuid: "fake-instance".to_string(),
                capabilities: Vec::new(),
                mcp_passthrough_version: MCP_PASSTHROUGH_VERSION,
                spool_max_format_version: local_rag_core::spool::FORMAT_VERSION,
                mode: "normal".to_string(),
            });
            let bytes = encode_message(&welcome).expect("encode WELCOME");
            writer.write_all(&bytes).expect("write WELCOME");

            if round < MAX_UPGRADE_ROUNDS {
                let mut shutdown_line = String::new();
                reader
                    .read_line(&mut shutdown_line)
                    .expect("read SHUTDOWN_REQUEST");
                let msg =
                    decode_message(shutdown_line.trim_end()).expect("decode SHUTDOWN_REQUEST");
                assert!(
                    matches!(msg, Message::ShutdownRequest(ShutdownRequest { .. })),
                    "expected a ShutdownRequest, got {msg:?}"
                );
            }
            // Dropping `reader`/`writer` here is this fake daemon's own
            // "drain and exit" — the real loop's `wait_for_close` observes
            // the resulting EOF.
        }
    })
}

/// The real `establish_session` upgrade loop, driven end to end: a fake
/// "old" daemon answers round 1 with a mismatched version, the real proxy
/// sends `SHUTDOWN_REQUEST` and waits for it to close, then — with the fake
/// listener's socket now unbound — `connect_or_spawn`'s round-2 attempt
/// genuinely fails to connect and spawns a real, current-version `local-rag
/// serve` daemon (inheriting this proxy's own `LOCAL_RAG_HOME`, exactly the
/// way `spawn_detached_daemon` always has), which the proxy then completes
/// a real MCP handshake against.
#[test]
fn daemon_version_mismatch_triggers_the_upgrade_flow_and_completes_against_the_new_daemon() {
    let home = TempHome::new().expect("temp home");
    let layout = open_layout(&home);

    let fake = spawn_fake_mismatched_daemon(layout.socket_path(), 1);

    let mut proxy = spawn_proxy(&home, &[("LOCAL_RAG_SESSION_ID", "test-session-upgrade")]);
    let mut stdin = proxy.stdin.take().expect("proxy stdin");
    let stdout = BufReader::new(proxy.stdout.take().expect("proxy stdout"));

    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    // Generous budget: this handshake pays for the fake round-trip *and* a
    // real daemon spawn, unlike the plain cold-start test's 20s.
    let (stdout, line) = read_line(&mut proxy, stdout, Duration::from_secs(25));
    let response: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse response");
    assert_eq!(
        response["result"]["serverInfo"]["name"], "local-rag",
        "the proxy must complete the handshake against the newly spawned, \
         version-matched daemon: {response}"
    );
    let _ = stdout;

    fake.join().expect("fake old daemon thread panicked");
    wait_until_daemon_ready(&layout, Duration::from_secs(5));

    drop(stdin);
    let status = wait_for_exit(&mut proxy, Duration::from_secs(20));
    assert!(status.success(), "proxy must exit 0: {status:?}");

    cleanup_daemon(&layout, Duration::from_secs(20));
}

/// A persistently mismatched daemon (every round, not just the first) makes
/// the proxy give up after `MAX_UPGRADE_ROUNDS` — `ProxyError::
/// UpgradeLoopExceeded`, exit non-zero, diagnostic on stderr — rather than
/// looping forever against a misbehaving/flapping peer.
#[test]
fn persistent_version_mismatch_exceeds_the_upgrade_round_budget() {
    let home = TempHome::new().expect("temp home");
    let layout = open_layout(&home);

    let fake = spawn_fake_mismatched_daemon(layout.socket_path(), MAX_UPGRADE_ROUNDS);

    let mut proxy = spawn_proxy(
        &home,
        &[("LOCAL_RAG_SESSION_ID", "test-session-upgrade-exceeded")],
    );
    let mut stderr = proxy.stderr.take().expect("proxy stderr");

    let status = wait_for_exit(&mut proxy, Duration::from_secs(25));
    assert!(
        !status.success(),
        "the proxy must exit non-zero once the upgrade round budget is exhausted: {status:?}"
    );

    let mut stderr_text = String::new();
    stderr
        .read_to_string(&mut stderr_text)
        .expect("read proxy stderr");
    assert!(
        stderr_text.contains("gave up after repeated version-mismatch upgrade attempts"),
        "{stderr_text}"
    );

    fake.join().expect("fake old daemon thread panicked");
}

/// The literal "daemon down/restart" scenario for a proxy that has not
/// started yet: a real daemon is killed ungracefully (`SIGKILL`, leaving a
/// stale, unbound `run/daemon.sock` behind — see [`send_sigkill`]'s own
/// doc), and a **fresh** proxy invocation against the same store must
/// transparently detect the dead owner, reach a brand-new daemon (spawning
/// one itself, unless the retiring proxy's own D-038 reconnect got there
/// first), and complete a real MCP handshake. The same restart under an
/// *already relaying* proxy is D-038's own scenario, covered separately by
/// [`a_live_proxy_reconnects_after_its_daemon_is_restarted_mid_session`].
#[test]
fn a_fresh_proxy_recovers_after_the_daemon_it_used_is_killed() {
    let home = TempHome::new().expect("temp home");
    let layout = open_layout(&home);

    let mut proxy1 = spawn_proxy(&home, &[("LOCAL_RAG_SESSION_ID", "test-session-restart-1")]);
    let mut stdin1 = proxy1.stdin.take().expect("proxy stdin");
    let stdout1 = BufReader::new(proxy1.stdout.take().expect("proxy stdout"));
    write_line(
        &mut stdin1,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    let (stdout1, _line) = read_line(&mut proxy1, stdout1, Duration::from_secs(20));
    let _ = stdout1;
    wait_until_daemon_ready(&layout, Duration::from_secs(5));

    let old_daemon = daemon_pid(&layout);
    send_sigkill(old_daemon);

    // Retire the first proxy before observing the kill: the daemon it spawned
    // is its *child*, and a child that has exited stays a zombie — answering
    // `kill(pid, 0)` like a live process — until someone reaps it. Since
    // D-038 the proxy no longer exits when its daemon dies (it reconnects, and
    // reaps on the way), so ending it here is what makes "the killed daemon is
    // gone" an observation about the daemon rather than a race with whatever
    // the surviving proxy does next.
    drop(stdin1);
    let _ = wait_for_exit(&mut proxy1, Duration::from_secs(30));

    let deadline = Instant::now() + Duration::from_secs(10);
    while pid_exists(old_daemon) {
        assert!(
            Instant::now() < deadline,
            "the killed daemon did not disappear within the timeout"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut proxy2 = spawn_proxy(&home, &[("LOCAL_RAG_SESSION_ID", "test-session-restart-2")]);
    let mut stdin2 = proxy2.stdin.take().expect("proxy stdin");
    let stdout2 = BufReader::new(proxy2.stdout.take().expect("proxy stdout"));
    write_line(
        &mut stdin2,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    let (stdout2, line) = read_line(&mut proxy2, stdout2, Duration::from_secs(20));
    let response: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse response");
    assert_eq!(
        response["result"]["serverInfo"]["name"], "local-rag",
        "a fresh proxy must recover against a brand-new daemon: {response}"
    );
    let _ = stdout2;

    wait_until_daemon_ready(&layout, Duration::from_secs(5));
    let new_daemon = daemon_pid(&layout);
    assert_ne!(
        new_daemon, old_daemon,
        "a genuinely new daemon process must have been spawned"
    );

    drop(stdin2);
    let status = wait_for_exit(&mut proxy2, Duration::from_secs(20));
    assert!(status.success(), "{status:?}");

    cleanup_daemon(&layout, Duration::from_secs(20));
}

// ---------------------------------------------------------------------
// D-038: an independently initiated daemon restart (`local-rag restart`/
// `stop`, a crash, an OOM kill) under an already-relaying proxy.
// ---------------------------------------------------------------------

/// Poll `store.lock` until it names a *different*, ready daemon than
/// `previous_pid` — i.e. until the proxy under test has reconnected far
/// enough to have spawned a replacement. Bounded like
/// [`wait_until_daemon_ready`], and for the same reason.
fn wait_for_replacement_daemon(layout: &StoreLayout, previous_pid: u32, timeout: Duration) -> u32 {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(bytes) = std::fs::read(layout.store_lock())
            && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && json.get("ready").and_then(|v| v.as_bool()) == Some(true)
            && let Some(pid) = json.get("pid").and_then(|v| v.as_u64())
            && pid as u32 != previous_pid
        {
            return pid as u32;
        }
        if Instant::now() >= deadline {
            panic!("no replacement daemon became ready within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// D-038's own scenario: a real daemon is `SIGTERM`ed in the middle of a
/// live relay session (exactly what `local-rag restart`/`stop` does — see
/// `cli::service::stop_running_daemon`, which signals the pid from
/// `store.lock` without going through the `ShutdownRequest` protocol, so a
/// connected proxy gets no warning at all). The proxy process must survive
/// it: reconnect to a freshly spawned daemon on its own, and serve the next
/// MCP tool call over the same stdin/stdout the client already holds.
#[test]
fn a_live_proxy_reconnects_after_its_daemon_is_restarted_mid_session() {
    let home = TempHome::new().expect("temp home");
    let layout = open_layout(&home);

    let mut proxy = spawn_proxy(&home, &[("LOCAL_RAG_SESSION_ID", "test-session-reconnect")]);
    let mut stdin = proxy.stdin.take().expect("proxy stdin");
    let stdout = BufReader::new(proxy.stdout.take().expect("proxy stdout"));

    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    let (stdout, line) = read_line(&mut proxy, stdout, Duration::from_secs(20));
    let response: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse response");
    assert_eq!(response["id"], serde_json::json!(1));

    wait_until_daemon_ready(&layout, Duration::from_secs(5));
    let old_daemon = daemon_pid(&layout);

    // The restart, exactly as `local-rag stop` performs it.
    send_sigterm(old_daemon);

    // The proxy notices on its own and spawns a replacement — nothing on
    // stdin prompts it. Waiting for that replacement before writing the next
    // request is what makes this test deterministic rather than a race
    // against the reconnect: a request written while the proxy still holds
    // the dead connection is a *different* scenario, covered by the in-flight
    // test below.
    let new_daemon = wait_for_replacement_daemon(&layout, old_daemon, Duration::from_secs(30));
    // The replacement can legitimately become ready while the outgoing daemon
    // is still finishing its own exit: an orderly drain releases the store
    // lock (spec 02 §4.3) before the process itself is reaped, which is
    // exactly what lets the successor bind at all. Bounded wait, not an
    // instant assertion.
    let deadline = Instant::now() + Duration::from_secs(20);
    while pid_exists(old_daemon) {
        assert!(
            Instant::now() < deadline,
            "the SIGTERMed daemon must exit, not merely be replaced in store.lock"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        proxy.try_wait().expect("try_wait").is_none(),
        "the proxy process must survive its daemon being restarted under it"
    );

    // The point of the whole exercise: the next tool call just works, with no
    // client-side intervention (no `/mcp` reconnect, no new proxy process).
    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let (stdout, line) = read_line(&mut proxy, stdout, Duration::from_secs(20));
    let response: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse response");
    assert_eq!(
        response["id"],
        serde_json::json!(2),
        "the first line after the restart must be this call's own response, \
         not a leftover error: {response}"
    );
    assert!(
        response["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .any(|t| t["name"] == serde_json::json!("search_code")),
        "{response}"
    );
    let _ = stdout;

    drop(stdin);
    let status = wait_for_exit(&mut proxy, Duration::from_secs(20));
    assert!(
        status.success(),
        "proxy must still exit 0 once stdin closes: {status:?}"
    );

    assert_eq!(daemon_pid(&layout), new_daemon);
    cleanup_daemon(&layout, Duration::from_secs(20));
}

/// A fake daemon that completes a version-matched handshake, reads exactly
/// one relayed request, and then drops the connection **without answering
/// it** — a daemon dying with a request in flight, deterministically, with
/// none of the timing luck real-process signalling would need. Dropping the
/// listener with it unbinds the socket, so the proxy's reconnect genuinely
/// falls through to spawning a real daemon.
fn spawn_fake_daemon_dying_mid_request(
    socket_path: std::path::PathBuf,
) -> std::thread::JoinHandle<()> {
    // D-111: bind on the CALLER's thread, not inside the spawned one. Every
    // caller does `spawn_fake_*(...)` and then immediately `spawn_proxy(...)`,
    // so a bind that happens inside the thread is racing the proxy's first
    // connect. Lose that race and the proxy finds no socket, does exactly what
    // it is supposed to do — spawns a REAL `local-rag serve` — and the test
    // then asserts against a real daemon's answer while the fake sits waiting
    // for a connection that never comes. It surfaced as a real daemon's
    // `initialize` result arriving where an error was required.
    //
    // The race was invisible for as long as `syspolicyd` charged 20-45 s for
    // the first exec of a freshly built binary: the thread always won by four
    // orders of magnitude. Removing that tax made the machine fast enough for
    // the proxy to win. Binding here removes the race rather than widening a
    // window: the socket exists before this function returns, so the caller's
    // `spawn_proxy` cannot precede it.
    let listener = UnixListener::bind(&socket_path).expect("bind fake daemon socket");
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept fake daemon connection");
        let mut reader = BufReader::new(stream.try_clone().expect("clone fake stream"));
        let mut writer = stream;

        let mut hello_line = String::new();
        reader.read_line(&mut hello_line).expect("read HELLO");

        let welcome = Message::Welcome(Welcome {
            proto: local_rag_protocol::PROTO_VERSION,
            daemon_version: local_rag_core::VERSION.to_string(),
            store_instance_uuid: "fake-instance".to_string(),
            capabilities: Vec::new(),
            mcp_passthrough_version: MCP_PASSTHROUGH_VERSION,
            spool_max_format_version: local_rag_core::spool::FORMAT_VERSION,
            mode: "normal".to_string(),
        });
        let bytes = encode_message(&welcome).expect("encode WELCOME");
        writer.write_all(&bytes).expect("write WELCOME");

        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .expect("read relayed request");
        // No response: this daemon dies holding the request.
    })
}

/// D-038's explicitly out-of-scope half, stated as a guarantee instead: a
/// request already in flight when the daemon dies is **not** replayed (this
/// proxy holds no session state to resume with, spec 11 §1) — it is failed,
/// promptly and in-band, so the client sees a retryable transport error
/// rather than a call that never returns. The session itself survives: the
/// very next call, on the same stdio, is answered by the reconnected daemon.
#[test]
fn a_request_in_flight_when_the_daemon_dies_fails_cleanly_and_the_session_continues() {
    let home = TempHome::new().expect("temp home");
    let layout = open_layout(&home);

    let fake = spawn_fake_daemon_dying_mid_request(layout.socket_path());

    let mut proxy = spawn_proxy(&home, &[("LOCAL_RAG_SESSION_ID", "test-session-in-flight")]);
    let mut stdin = proxy.stdin.take().expect("proxy stdin");
    let stdout = BufReader::new(proxy.stdout.take().expect("proxy stdout"));

    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    // The bounded read is the assertion that matters most here: without the
    // synthesized error this call would hang until the test's own timeout.
    let (stdout, line) = read_line(&mut proxy, stdout, Duration::from_secs(20));
    let response: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse response");
    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(
        response["error"]["code"],
        serde_json::json!(-32000),
        "an unanswered in-flight request must come back as a JSON-RPC error: {response}"
    );
    assert!(response.get("result").is_none(), "{response}");

    fake.join().expect("fake daemon thread panicked");

    // The proxy reconnects on its own — the fake listener is unbound by now,
    // so this spawns a real `local-rag serve` — and the retry the client is
    // free to send is answered normally.
    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}"#,
    );
    let (stdout, line) = read_line(&mut proxy, stdout, Duration::from_secs(30));
    let response: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse response");
    assert_eq!(response["id"], serde_json::json!(2));
    assert_eq!(
        response["result"]["serverInfo"]["name"], "local-rag",
        "the retry must be answered by the reconnected, real daemon: {response}"
    );
    let _ = stdout;

    wait_until_daemon_ready(&layout, Duration::from_secs(5));

    drop(stdin);
    let status = wait_for_exit(&mut proxy, Duration::from_secs(20));
    assert!(status.success(), "proxy must exit 0: {status:?}");

    cleanup_daemon(&layout, Duration::from_secs(20));
}

// ---------------------------------------------------------------------
// T17-04: proxy-side spool `format_version` compatibility warning
// (spec 11 §4 `[FIXED concern]`).
// ---------------------------------------------------------------------

/// A minimal, blocking fake daemon: binds `socket_path`, answers one
/// connection with a `Welcome` whose `daemon_version` **matches** this
/// workspace's real build (so `establish_session` never enters the upgrade
/// loop) but whose `spool_max_format_version` is `spool_max_format_version`
/// — then answers exactly one relayed MCP request with a trivial, valid
/// `Response`, so a test can prove the real relay loop still runs a clean
/// JSON-RPC round trip on stdout alongside whatever this proxy prints to
/// stderr about the mismatch.
///
/// It then holds the connection open until the proxy itself closes it. Since
/// D-038 the proxy treats a daemon disappearing as something to reconnect to
/// (spawning a real one, here pointlessly), so a fake that hung up early
/// would drag an unrelated daemon start into a test about a stderr warning.
fn spawn_fake_daemon_with_spool_version(
    socket_path: std::path::PathBuf,
    spool_max_format_version: u16,
) -> std::thread::JoinHandle<()> {
    // D-111: bind on the CALLER's thread, not inside the spawned one. Every
    // caller does `spawn_fake_*(...)` and then immediately `spawn_proxy(...)`,
    // so a bind that happens inside the thread is racing the proxy's first
    // connect. Lose that race and the proxy finds no socket, does exactly what
    // it is supposed to do — spawns a REAL `local-rag serve` — and the test
    // then asserts against a real daemon's answer while the fake sits waiting
    // for a connection that never comes. It surfaced as a real daemon's
    // `initialize` result arriving where an error was required.
    //
    // The race was invisible for as long as `syspolicyd` charged 20-45 s for
    // the first exec of a freshly built binary: the thread always won by four
    // orders of magnitude. Removing that tax made the machine fast enough for
    // the proxy to win. Binding here removes the race rather than widening a
    // window: the socket exists before this function returns, so the caller's
    // `spawn_proxy` cannot precede it.
    let listener = UnixListener::bind(&socket_path).expect("bind fake daemon socket");
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept fake daemon connection");
        let mut reader = BufReader::new(stream.try_clone().expect("clone fake stream"));
        let mut writer = stream;

        let mut hello_line = String::new();
        reader.read_line(&mut hello_line).expect("read HELLO");

        let welcome = Message::Welcome(Welcome {
            proto: local_rag_protocol::PROTO_VERSION,
            daemon_version: local_rag_core::VERSION.to_string(),
            store_instance_uuid: "fake-instance".to_string(),
            capabilities: Vec::new(),
            mcp_passthrough_version: MCP_PASSTHROUGH_VERSION,
            spool_max_format_version,
            mode: "normal".to_string(),
        });
        let bytes = encode_message(&welcome).expect("encode WELCOME");
        writer.write_all(&bytes).expect("write WELCOME");

        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .expect("read relayed request");
        let response = Message::Response(local_rag_protocol::ResponseEnvelope {
            mcp: serde_json::value::RawValue::from_string(
                r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#.to_string(),
            )
            .expect("valid raw json"),
        });
        let bytes = encode_message(&response).expect("encode Response");
        writer.write_all(&bytes).expect("write Response");

        let mut drained = String::new();
        let _ = reader.read_line(&mut drained); // returns once the proxy exits
    })
}

/// A daemon advertising a `spool_max_format_version` older than this
/// release's own compiled `local_rag_core::spool::FORMAT_VERSION` produces a
/// stderr warning naming both versions, while stdout carries only the real
/// JSON-RPC response — never corrupted by the warning (spec 11 §4 `[FIXED
/// concern]`, the proxy-side half T15-02's own as-built note named as
/// remaining later work).
#[test]
fn a_daemon_advertising_an_older_spool_format_produces_a_stderr_warning_and_never_touches_stdout() {
    let home = TempHome::new().expect("temp home");
    let layout = open_layout(&home);

    // `0` is always older than any real `FORMAT_VERSION` (currently `1`),
    // without hardcoding today's exact value in the test.
    let fake = spawn_fake_daemon_with_spool_version(layout.socket_path(), 0);

    let mut proxy = spawn_proxy(
        &home,
        &[("LOCAL_RAG_SESSION_ID", "test-session-spool-warning")],
    );
    let mut stdin = proxy.stdin.take().expect("proxy stdin");
    let stdout = BufReader::new(proxy.stdout.take().expect("proxy stdout"));

    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    let (stdout, line) = read_line(&mut proxy, stdout, Duration::from_secs(20));
    // stdout carries exactly the fake daemon's response and nothing else —
    // proving the warning never touched the JSON-RPC stream.
    let response: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse response");
    assert_eq!(response["result"]["ok"], serde_json::json!(true));
    let _ = stdout;

    drop(stdin);
    let status = wait_for_exit(&mut proxy, Duration::from_secs(20));
    assert!(
        status.success(),
        "proxy must exit 0 once stdin closes: {status:?}"
    );

    let mut stderr_buf = Vec::new();
    proxy
        .stderr
        .take()
        .expect("proxy stderr")
        .read_to_end(&mut stderr_buf)
        .expect("read stderr");
    let stderr = String::from_utf8_lossy(&stderr_buf);
    assert!(stderr.contains("spool format versions"), "{stderr}");
    assert!(
        stderr.contains(&local_rag_core::spool::FORMAT_VERSION.to_string()),
        "{stderr}"
    );
    assert!(stderr.contains('0'), "{stderr}");

    fake.join().expect("fake daemon thread panicked");
}

// ---------------------------------------------------------------------
// T17-04: a genuine cross-binary-version upgrade, with a real migration
// running on the new side (spec 13 §4's upgrade flow, end to end).
// ---------------------------------------------------------------------

/// A real, three-process upgrade: an "old daemon" is a genuinely compiled
/// `local-rag serve`, configured via T17-04's `failpoints`-gated env-var
/// overrides (`main.rs::test_daemon_version_override`,
/// `local_rag_store::state::migration_set_for_this_open`'s equivalent) to
/// answer a fake, mismatched `daemon_version` and to migrate the store only
/// through a **restricted** schema version — standing in for "an older
/// release" without a second historical binary or machine (none is
/// available here: no network, no second checkout). A **clean-environment**
/// real proxy then drives the real `establish_session` upgrade loop against
/// it: `SHUTDOWN_REQUEST` → the old daemon drains and exits → a **second**,
/// unrestricted real `local-rag serve` process is spawned (inheriting the
/// proxy's own clean environment, not the old daemon's) → a real MCP
/// handshake completes against it. The real-migration proof is a subsequent
/// `local-rag doctor --json` (also clean environment) against the same
/// on-disk store: `store_version` must have advanced past the old daemon's
/// restricted cap with nothing left pending — proof the *second* process
/// genuinely migrated what the *first* process left behind, not merely that
/// the protocol-level handshake retried.
///
/// Gated on `failpoints`: run via
/// `cargo test -p local-rag-proxy --features failpoints`.
#[test]
#[cfg(feature = "failpoints")]
fn a_real_older_daemon_binary_drains_and_a_real_new_daemon_migrates_the_store_to_head() {
    const OLD_DAEMON_MAX_SCHEMA_VERSION: &str = "8";

    let home = TempHome::new().expect("temp home");
    let layout = open_layout(&home);

    let mut old_daemon_cmd = home.command(local_rag_binary_path());
    old_daemon_cmd.arg("serve");
    old_daemon_cmd.env("LOCAL_RAG_TEST_FAKE_DAEMON_VERSION", "0.0.0-legacy");
    old_daemon_cmd.env(
        "LOCAL_RAG_TEST_MAX_SCHEMA_VERSION",
        OLD_DAEMON_MAX_SCHEMA_VERSION,
    );
    old_daemon_cmd.stdin(Stdio::null());
    old_daemon_cmd.stdout(Stdio::piped());
    old_daemon_cmd.stderr(Stdio::piped());
    let mut old_daemon = old_daemon_cmd.spawn().expect("spawn old daemon");
    wait_until_daemon_ready(&layout, Duration::from_secs(20));

    // The real proxy, with a CLEAN environment (no override vars): its own
    // compiled `local_rag_core::VERSION` differs from "0.0.0-legacy",
    // triggering the real upgrade flow.
    let mut proxy = spawn_proxy(
        &home,
        &[("LOCAL_RAG_SESSION_ID", "test-session-cross-binary-upgrade")],
    );
    let mut stdin = proxy.stdin.take().expect("proxy stdin");
    let stdout = BufReader::new(proxy.stdout.take().expect("proxy stdout"));

    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    // Generous budget: pays for the old daemon's drain *and* a real new
    // daemon spawn+migration.
    let (stdout, line) = read_line(&mut proxy, stdout, Duration::from_secs(30));
    let response: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("parse response");
    assert_eq!(
        response["result"]["serverInfo"]["name"], "local-rag",
        "the proxy must complete the handshake against the newly spawned daemon: {response}"
    );
    let _ = stdout;

    let old_status = wait_for_exit(&mut old_daemon, Duration::from_secs(20));
    assert!(
        old_status.success(),
        "the old daemon must drain and exit cleanly on SHUTDOWN_REQUEST: {old_status:?}"
    );

    wait_until_daemon_ready(&layout, Duration::from_secs(5));
    drop(stdin);
    let status = wait_for_exit(&mut proxy, Duration::from_secs(20));
    assert!(status.success(), "proxy must exit 0: {status:?}");

    // Real-migration proof: a clean-environment `local-rag doctor --json`
    // against the same on-disk store.
    let mut doctor_cmd = home.command(local_rag_binary_path());
    doctor_cmd.args(["doctor", "--json"]);
    doctor_cmd.stdin(Stdio::null());
    doctor_cmd.stdout(Stdio::piped());
    doctor_cmd.stderr(Stdio::piped());
    let doctor_output = doctor_cmd.output().expect("run local-rag doctor");
    let report: serde_json::Value =
        serde_json::from_slice(&doctor_output.stdout).expect("valid doctor json");
    assert_eq!(
        report["versions"]["state"], "applied",
        "the store must be fully migrated, not left pending: {report}"
    );
    assert_eq!(
        report["versions"]["pending"],
        serde_json::json!([]),
        "{report}"
    );
    let store_version = report["versions"]["store_version"]
        .as_u64()
        .expect("store_version is a number");
    let old_cap: u64 = OLD_DAEMON_MAX_SCHEMA_VERSION.parse().unwrap();
    assert!(
        store_version > old_cap,
        "the new daemon must have genuinely migrated past the old daemon's own \
         restricted cap ({old_cap}), not merely re-served the same version: {report}"
    );

    cleanup_daemon(&layout, Duration::from_secs(20));
}
