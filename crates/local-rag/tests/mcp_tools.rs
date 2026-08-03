//! T15-03 store-backed MCP tool tests: explicit context routing, unknown
//! worktree behavior, and no synchronous indexing on the MCP path.

mod support;

use std::time::Duration;

use serde_json::Value;
use support::{Client, git_available, open_layout, seed_indexed_worktree, start};

/// Two `tools/call`s on **one** connection, differing only in the
/// context's `worktree_root`: a real, seeded, indexed worktree must
/// succeed, and a real-but-unregistered directory must not — proving the
/// context is applied per request, not cached or shared across calls on
/// the same connection. Extends T15-02's own `two_requests_on_one_
/// connection_keep_their_own_context` (proven there against the echo
/// stub) to the real handler and real domain outcomes.
#[tokio::test]
async fn explicit_context_routing_across_two_requests_on_one_connection() {
    if !git_available() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (home, layout) = open_layout();
    let seeded = seed_indexed_worktree(&home, &layout).await;
    let other_dir = home.join("unregistered");
    std::fs::create_dir_all(&other_dir).expect("create unregistered dir");

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;

    let repo_path = seeded.repo_path.to_string_lossy().into_owned();
    let other_path = other_dir.to_string_lossy().into_owned();

    let (indexed, unregistered) = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        let indexed = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"project_overview","arguments":{}}}"#,
            Some(&repo_path),
        );
        let unregistered = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"project_overview","arguments":{}}}"#,
            Some(&other_path),
        );
        (indexed, unregistered)
    })
    .await
    .expect("blocking task");

    assert_eq!(
        indexed["result"]["isError"],
        Value::Bool(false),
        "the seeded, indexed worktree must succeed: {indexed}"
    );
    let overview_text = indexed["result"]["content"][0]["text"].as_str().unwrap();
    assert!(overview_text.contains("\"generation\""), "{overview_text}");

    assert_eq!(
        unregistered["result"]["isError"],
        Value::Bool(true),
        "an unregistered worktree on the very next call must not see the previous one's context: {unregistered}"
    );
    let error_text = unregistered["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        error_text.contains("\"code\":\"WORKTREE_NOT_INDEXED\""),
        "{error_text}"
    );

    handle.shutdown().await;
}

/// A real, existing directory that was never registered, and a path that
/// does not exist at all, both resolve to `WORKTREE_NOT_INDEXED` — a
/// normal tool result (`isError: true`), never a JSON-RPC-level error and
/// never a hard failure. Spec 02 §3.3: "an unresolvable root resolves to
/// `GlobalOnly` — never an error."
#[tokio::test]
async fn an_unknown_worktree_is_worktree_not_indexed_never_an_error() {
    let (home, layout) = open_layout();
    let socket_path = layout.socket_path();
    let handle = start(&layout).await;

    let real_dir = home.join("never-registered");
    std::fs::create_dir_all(&real_dir).expect("create dir");
    let real_path = real_dir.to_string_lossy().into_owned();
    let missing_path = home
        .join("does-not-exist-at-all")
        .to_string_lossy()
        .into_owned();

    let (real_body, missing_body) = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        let real = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"project_overview","arguments":{}}}"#,
            Some(&real_path),
        );
        let missing = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"project_overview","arguments":{}}}"#,
            Some(&missing_path),
        );
        (real, missing)
    })
    .await
    .expect("blocking task");

    for body in [&real_body, &missing_body] {
        assert_eq!(
            body["error"],
            Value::Null,
            "must be a tool result, not a JSON-RPC error: {body}"
        );
        assert_eq!(body["result"]["isError"], Value::Bool(true), "{body}");
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"code\":\"WORKTREE_NOT_INDEXED\""), "{text}");
    }

    handle.shutdown().await;
}

/// A never-indexed worktree answers promptly — bounded by a timeout, not a
/// wall-clock measurement — proving the MCP path never blocks on or
/// triggers indexing work (this card's own "no synchronous indexing
/// call"). Structurally guaranteed too: `daemon::search::
/// NoRebuildVectorSource` can never supply a vector, so nothing on this
/// path *could* rebuild a shard even if it tried.
#[tokio::test]
async fn a_never_indexed_worktree_responds_promptly() {
    let (home, layout) = open_layout();
    let socket_path = layout.socket_path();
    let handle = start(&layout).await;

    let real_dir = home.join("never-indexed");
    std::fs::create_dir_all(&real_dir).expect("create dir");
    let path = real_dir.to_string_lossy().into_owned();

    let body = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            let mut client = Client::connect(&socket_path);
            client.call_and_read(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_code","arguments":{"query":"anything"}}}"#,
                Some(&path),
            )
        }),
    )
    .await
    .expect("must respond within the timeout: a synchronous indexing attempt would hang past it")
    .expect("blocking task");

    assert_eq!(body["result"]["isError"], Value::Bool(true));
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("\"code\":\"WORKTREE_NOT_INDEXED\""), "{text}");

    handle.shutdown().await;
}

/// `get_file_context`'s own MCP-dispatch-level contract (G15/D-026: unlike
/// `search_code`/`project_overview` above, this tool had no store-backed
/// `tools/call` test at all before this — only its private path-helper and
/// the underlying `SearchEngine` method were covered). Real, seeded,
/// indexed worktree; asserts the occurrence list names the seeded path.
#[tokio::test]
async fn get_file_context_returns_the_seeded_occurrence() {
    if !git_available() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (home, layout) = open_layout();
    let seeded = seed_indexed_worktree(&home, &layout).await;

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let repo_path = seeded.repo_path.to_string_lossy().into_owned();

    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_file_context","arguments":{"path":"src/lib.rs"}}}"#,
            Some(&repo_path),
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(body["result"]["isError"], Value::Bool(false), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["path"], "src/lib.rs", "{text}");
    let occurrences = parsed["occurrences"].as_array().expect("occurrences array");
    assert!(!occurrences.is_empty(), "{text}");

    handle.shutdown().await;
}
