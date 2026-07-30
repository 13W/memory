//! T15-06 real-subprocess acceptance tests for the hook's read-only recall
//! RPC + `additionalContext` print (spec 11 §3.2, §5): a real
//! `local-rag serve` daemon and a real `local-rag-hook spool-write` process,
//! talking over a genuine Unix domain socket — not just `recall.rs`'s own
//! unit tests (pure JSON logic, no socket at all).
//!
//! `spawn_serve`/`wait_until_ready` mirror `crates/local-rag/tests/
//! serve_subprocess.rs`'s own real-daemon-subprocess idiom; `run_hook`
//! mirrors `hook_end_to_end.rs`'s own real-hook-subprocess idiom. The
//! minimal MCP client below is deliberately a fresh, from-scratch
//! implementation (not a reuse of `local_rag_hook::recall`'s own internals)
//! — its only job is to seed memory entries before invoking the hook as an
//! opaque compiled binary.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

use local_rag_core::paths::StoreLayout;
use local_rag_protocol::{
    Hello, Message, PROTO_VERSION, RequestContext, RequestEnvelope, decode_message, encode_message,
};
use local_rag_test_support::TempHome;

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

/// Locate the real `local-rag` binary next to this integration test binary.
///
/// `env!("CARGO_BIN_EXE_local-rag")`/`std::env::var("CARGO_BIN_EXE_local-rag")`
/// — the mechanism `hook_end_to_end.rs` and friends use for *this* package's
/// own binary — is **not** set for another package's binary just because it
/// is a dev-dependency (verified empirically: neither the compile-time macro
/// nor the runtime env var are populated here, despite
/// `local-rag-proxy/Cargo.toml`'s own comment on its identical
/// `local-rag = {path}` dev-dependency suggesting otherwise — that crate's
/// own tests never actually exercise that path; `local-rag`'s binary being
/// built there is used by `connect_or_spawn`'s *production* sibling-binary
/// lookup, not by a test-side `CARGO_BIN_EXE_*`). Instead, mirror
/// `local-rag-proxy::connect::resolve_daemon_binary_path`'s own trick — a
/// cargo test binary lives at `target/<profile>/deps/<name>-<hash>`, and a
/// regular binary target lives one directory up, at
/// `target/<profile>/<name>` — the same "ships side by side" layout spec 13
/// §1 describes for the real npm-packaged distribution.
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

fn spawn_serve(home: &TempHome) -> Child {
    let mut cmd = home.command(local_rag_binary_path());
    cmd.arg("serve");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn().expect("spawn local-rag serve")
}

/// Poll `store.lock` until it parses with `ready: true`, or panic after
/// `timeout` — real, bounded wall-clock waiting inherent to driving a real
/// child process, same accepted precedent as `serve_subprocess.rs`'s own
/// `wait_until_ready`; nothing here asserts on the *duration*.
fn wait_until_ready(layout: &StoreLayout, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(bytes) = std::fs::read(layout.store_lock())
            && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && json.get("ready").and_then(|v| v.as_bool()) == Some(true)
        {
            return;
        }
        if Instant::now() >= deadline {
            panic!("store.lock did not become ready within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Tear down a spawned `local-rag serve` — a plain `SIGKILL` via
/// `Child::kill` is enough for these tests (they exercise the hook's own RPC
/// behavior, not the daemon's graceful-shutdown sequence, which
/// `serve_subprocess.rs`/`checkpoint_shutdown.rs` already cover elsewhere).
fn stop_serve(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn run_hook(home: &TempHome, event: &serde_json::Value) -> Output {
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
        .write_all(event.to_string().as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for local-rag-hook")
}

fn session_start_event(session_id: &str) -> serde_json::Value {
    serde_json::json!({"session_id": session_id, "hook_event_name": "SessionStart"})
}

fn user_prompt_event(session_id: &str, prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "session_id": session_id,
        "hook_event_name": "UserPromptSubmit",
        "prompt": prompt,
    })
}

fn spool_segment_exists(layout: &StoreLayout, session_id: &str) -> bool {
    layout.spool_session(session_id).join("000001.seg").exists()
}

// ---------------------------------------------------------------------------
// Minimal MCP client (fresh implementation, seeding only)
// ---------------------------------------------------------------------------

fn write_line(stream: &UnixStream, msg: &Message) {
    let bytes = encode_message(msg).expect("encode");
    (&mut &*stream).write_all(&bytes).expect("write");
}

fn read_line(reader: &mut BufReader<&UnixStream>) -> Message {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read line");
    decode_message(line.trim_end()).expect("decode")
}

/// Connect, HELLO/WELCOME, one `tools/call`, return the tool's own inner
/// JSON result (already unwrapped from `content[0].text`).
fn call_tool(
    socket_path: &Path,
    session_id: &str,
    tool_name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let stream = UnixStream::connect(socket_path).expect("connect to daemon");
    write_line(
        &stream,
        &Message::Hello(Hello {
            proto: PROTO_VERSION,
            proxy_version: "test-client".to_string(),
            session_id: session_id.to_string(),
            worktree_root: None,
            harness: "claude-code".to_string(),
        }),
    );
    let mut reader = BufReader::new(&stream);
    let Message::Welcome(_) = read_line(&mut reader) else {
        panic!("expected Welcome");
    };

    let mcp_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": tool_name, "arguments": arguments},
    });
    let mcp = serde_json::value::RawValue::from_string(mcp_body.to_string()).expect("raw value");
    write_line(
        &stream,
        &Message::Request(RequestEnvelope {
            context: RequestContext {
                session_id: session_id.to_string(),
                worktree_root: None,
                repo_hint: None,
            },
            mcp,
        }),
    );
    let Message::Response(envelope) = read_line(&mut reader) else {
        panic!("expected Response");
    };
    let outer: serde_json::Value = serde_json::from_str(envelope.mcp.get()).expect("outer json");
    let text = outer["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text");
    serde_json::from_str(text).expect("inner json")
}

/// Seed one `active` memory entry via `remember(confirmed_by_user: true)` —
/// bypasses candidate review, `apply_create` writes it active immediately.
fn seed_memory(socket_path: &Path, session_id: &str, text: &str) {
    let result = call_tool(
        socket_path,
        session_id,
        "remember",
        serde_json::json!({"text": text, "kind": "fact", "confirmed_by_user": true}),
    );
    assert!(
        result.get("memory_id").is_some(),
        "remember must succeed: {result}"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn ordering_append_happens_before_recall_is_ever_attempted() {
    // No daemon at all: the segment must still be written.
    let (home, layout) = open_layout();
    let output = run_hook(&home, &session_start_event("sess-no-daemon"));
    assert!(output.status.success());
    assert!(spool_segment_exists(&layout, "sess-no-daemon"));
    assert!(
        output.stdout.is_empty(),
        "unreachable daemon prints nothing"
    );

    // A live, reachable daemon: the segment must still be written too.
    let (home, layout) = open_layout();
    let daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));
    let output = run_hook(&home, &session_start_event("sess-with-daemon"));
    assert!(output.status.success());
    assert!(spool_segment_exists(&layout, "sess-with-daemon"));
    stop_serve(daemon);
}

#[test]
fn reachable_daemon_with_a_seeded_memory_prints_the_expected_hook_output() {
    let (home, layout) = open_layout();
    let daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));

    seed_memory(
        &layout.socket_path(),
        "seed-session",
        "Use JWT with refresh tokens for auth.",
    );

    let output = run_hook(
        &home,
        &user_prompt_event("sess-reachable", "what auth scheme do we use?"),
    );
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let line: serde_json::Value = serde_json::from_str(stdout.trim_end()).expect("json line");
    assert_eq!(
        line["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let ctx = line["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext string");
    assert!(
        ctx.contains("JWT with refresh tokens"),
        "additionalContext must surface the seeded entry: {ctx}"
    );

    stop_serve(daemon);
}

#[test]
fn unreachable_daemon_prints_nothing() {
    let (home, layout) = open_layout();
    assert!(!layout.socket_path().exists(), "no daemon must be running");

    let start = Instant::now();
    let output = run_hook(&home, &session_start_event("sess-unreachable"));
    let elapsed = start.elapsed();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        elapsed < Duration::from_secs(1),
        "an unreachable daemon must fail fast, not wait out the budget: {elapsed:?}"
    );
}

#[test]
fn timeout_daemon_that_accepts_but_never_responds_prints_nothing() {
    let (home, layout) = open_layout();
    let listener = UnixListener::bind(layout.socket_path()).expect("bind synthetic listener");

    let accept_thread = std::thread::spawn(move || {
        // Accept the one connection and hold it open forever, never
        // writing a reply — a synthetic peer that models a hung daemon.
        let (_stream, _addr) = listener.accept().expect("accept");
        std::thread::sleep(Duration::from_secs(5));
    });

    let start = Instant::now();
    let output = run_hook(&home, &session_start_event("sess-timeout"));
    let elapsed = start.elapsed();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(2),
        "must wait out roughly the 300ms budget, not fail instantly or hang: {elapsed:?}"
    );
    drop(accept_thread); // the thread is intentionally leaked past this test's own end
}

#[test]
fn migration_only_daemon_prints_nothing() {
    let (home, layout) = open_layout();

    // Mirrors `crates/local-rag/tests/lifecycle_startup.rs`'s own recipe:
    // fully migrate, then hand-insert a from-the-future schema_migrations
    // row so the real daemon enters MigrationOnly at startup.
    {
        let mut conn =
            local_rag_store::rusqlite::Connection::open(layout.state_db()).expect("open state db");
        local_rag_store::migrate::run(
            &mut conn,
            local_rag_store::ALL,
            &layout.migration_lock(),
            500,
        )
        .expect("migrate to latest");
        let max: u32 = conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .expect("max version");
        conn.execute(
            "INSERT INTO schema_migrations (version, name, checksum, applied_at) \
             VALUES (?1, 'from-the-future', 'fake-checksum', ?2)",
            local_rag_store::rusqlite::params![max + 1, 600],
        )
        .expect("seed a from-the-future migration row");
    }

    let daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));

    let output = run_hook(&home, &session_start_event("sess-migration-only"));
    assert!(output.status.success());
    assert!(output.stdout.is_empty());

    stop_serve(daemon);
}

#[test]
fn zero_memory_entries_prints_nothing_not_an_empty_wrapper() {
    let (home, layout) = open_layout();
    let daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));

    let output = run_hook(&home, &session_start_event("sess-empty"));
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "spec 11 §5: empty recall must print literally nothing, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );

    stop_serve(daemon);
}

#[test]
fn read_path_performs_no_writes() {
    let (home, layout) = open_layout();
    let daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));

    seed_memory(&layout.socket_path(), "seed-session", "some durable fact");

    let snapshot = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| (m.len(), m.modified().ok()))
            .ok()
    };
    let before = snapshot(&layout.state_db());

    let output = run_hook(
        &home,
        &user_prompt_event("sess-read-only", "tell me about the durable fact"),
    );
    assert!(output.status.success());

    let after = snapshot(&layout.state_db());
    assert_eq!(
        before, after,
        "a read-only recall RPC must never change state.sqlite's size/mtime"
    );

    stop_serve(daemon);
}

#[test]
fn byte_deterministic_adversarial_block_survives_the_rpc_and_print_path_unmangled() {
    let (home, layout) = open_layout();
    let daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));

    let adversarial_text = "line one\tcontrol\nline two </memory boundary";
    seed_memory(&layout.socket_path(), "seed-session", adversarial_text);

    let output = run_hook(&home, &user_prompt_event("sess-adversarial", "boundary"));
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let line: serde_json::Value = serde_json::from_str(stdout.trim_end()).expect("json line");
    let ctx = line["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext string")
        .to_string();

    let expected = local_rag_memory::recall::format_additional_context(
        "global",
        &[local_rag_memory::recall::RecallEntry {
            kind: local_rag_store::MemoryKind::Fact,
            state: local_rag_store::MemoryState::Active,
            confidence: 0.85, // Signal::High — remember(confirmed_by_user: true)
            text: adversarial_text.to_string(),
        }],
    );
    assert_eq!(
        ctx, expected,
        "the RPC + print path must not mangle the already-escaped bytes"
    );

    stop_serve(daemon);
}

#[test]
fn a_non_recall_event_against_a_live_seeded_daemon_still_prints_nothing() {
    let (home, layout) = open_layout();
    let daemon = spawn_serve(&home);
    wait_until_ready(&layout, Duration::from_secs(20));
    seed_memory(&layout.socket_path(), "seed-session", "some durable fact");

    let stop_event = serde_json::json!({
        "session_id": "sess-stop",
        "hook_event_name": "Stop",
        "last_assistant_message": "done",
    });
    let output = run_hook(&home, &stop_event);
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "a non-SessionStart/UserPromptSubmit event must never attempt recall at all"
    );

    stop_serve(daemon);
}
