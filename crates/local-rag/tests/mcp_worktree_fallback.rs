//! D-137 (ADR-0016): the per-call worktree fallback, end to end against a
//! real daemon. `RequestContext.worktree_fallback` is consulted only when the
//! launch `worktree_root` resolves to `GlobalOnly`; the launch context always
//! wins, and a fallback that does not resolve either is global scope, never an
//! error.

#![cfg(unix)]

mod support;

use serde_json::Value;

use local_rag_protocol::RequestContext;
use local_rag_store::{GLOBAL_SCOPE_OWNER_ID, ScopeKind, StateDb};
use support::{
    Client, git_available, open_layout, register_worktree, seed_indexed_worktree, start,
};

fn context(worktree_root: Option<&str>, fallback: Option<&str>) -> RequestContext {
    RequestContext {
        session_id: String::new(), // replaced by the client's own
        worktree_root: worktree_root.map(str::to_string),
        repo_hint: None,
        worktree_fallback: fallback.map(str::to_string),
    }
}

/// `id` must differ per call on one connection: `remember` is idempotent per
/// `(session_id, request id)`, so a reused id is a retry of the first call.
fn remember(id: u32, text: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"remember","arguments":{{"text":"{text}","kind":"fact"}}}}}}"#
    )
}

/// The tool result's own JSON payload.
fn payload(body: &Value) -> Value {
    assert_eq!(body["result"]["isError"], Value::Bool(false), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

/// `(scope_kind, scope_owner_id)` of the entry holding `text`.
fn owner_of(state: &StateDb, text: &str) -> (String, String) {
    let read = state.open_read().expect("read conn");
    read.query_row(
        "SELECT scope_kind, scope_owner_id FROM memory_entry WHERE text = ?1",
        [text],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .expect("the entry was created")
}

/// Rule 1: a launch root that resolves owns routing — a fallback naming a
/// different, equally registered repository is ignored.
#[tokio::test]
async fn a_resolving_launch_root_ignores_the_fallback() {
    if !git_available() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (home, layout) = open_layout();
    let launch = seed_indexed_worktree(&home, &layout).await;
    let other = register_worktree(&home, &layout, "other-repo").await;
    let launch_path = launch.repo_path.to_string_lossy().into_owned();
    let other_path = other.repo_path.to_string_lossy().into_owned();

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let body = tokio::task::spawn_blocking(move || {
        Client::connect(&socket_path).call_with_context_and_read(
            &remember(1, "launch wins"),
            context(Some(&launch_path), Some(&other_path)),
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(payload(&body)["scope"], "repository", "{body}");
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    assert_eq!(
        owner_of(&state, "launch wins"),
        (ScopeKind::Repository.as_str().to_string(), launch.repo_id)
    );

    handle.shutdown().await;
}

/// Rule 2: a `GlobalOnly` launch root — none at all, or a directory the
/// registry does not know — hands routing to a fallback that resolves:
/// `remember` writes repository scope there, and the code tools see the
/// fallback's index instead of `WORKTREE_NOT_INDEXED`.
#[tokio::test]
async fn a_global_only_launch_root_routes_to_a_resolving_fallback() {
    if !git_available() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (home, layout) = open_layout();
    let seeded = seed_indexed_worktree(&home, &layout).await;
    let fallback = seeded.repo_path.to_string_lossy().into_owned();
    let unknown_dir = home.join("cowork-app-dir");
    std::fs::create_dir_all(&unknown_dir).expect("create dir");
    let unknown = unknown_dir.to_string_lossy().into_owned();

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let (no_launch, unknown_launch, overview) = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        let no_launch = client
            .call_with_context_and_read(&remember(1, "no launch root"), context(None, Some(&fallback)));
        let unknown_launch = client.call_with_context_and_read(
            &remember(2, "unknown launch root"),
            context(Some(&unknown), Some(&fallback)),
        );
        let overview = client.call_with_context_and_read(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"project_overview","arguments":{}}}"#,
            context(Some(&unknown), Some(&fallback)),
        );
        (no_launch, unknown_launch, overview)
    })
    .await
    .expect("blocking task");

    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    for (body, text) in [
        (&no_launch, "no launch root"),
        (&unknown_launch, "unknown launch root"),
    ] {
        let parsed = payload(body);
        assert_eq!(parsed["scope"], "repository", "{body}");
        assert!(parsed.get("degraded").is_none(), "{body}");
        assert_eq!(
            owner_of(&state, text),
            (
                ScopeKind::Repository.as_str().to_string(),
                seeded.repo_id.clone()
            )
        );
    }

    assert_eq!(
        overview["result"]["isError"],
        Value::Bool(false),
        "{overview}"
    );
    let overview_text = overview["result"]["content"][0]["text"].as_str().unwrap();
    assert!(overview_text.contains("\"generation\""), "{overview_text}");

    handle.shutdown().await;
}

/// A fallback that does not resolve either — a relative path, a path that
/// does not exist, a directory the registry does not know — leaves the call
/// in global scope, exactly as without one. Never a new error.
#[tokio::test]
async fn an_unresolvable_fallback_stays_global_without_an_error() {
    let (home, layout) = open_layout();
    let unknown_dir = home.join("never-registered");
    std::fs::create_dir_all(&unknown_dir).expect("create dir");
    let unknown = unknown_dir.to_string_lossy().into_owned();

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let bodies = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        [
            (1, "relative", "relative/path".to_string()),
            (
                2,
                "missing",
                "/definitely/does/not/exist/xyz-137".to_string(),
            ),
            (3, "unregistered", unknown),
        ]
        .map(|(id, label, fallback)| {
            (
                label,
                client.call_with_context_and_read(
                    &remember(id, label),
                    context(None, Some(&fallback)),
                ),
            )
        })
    })
    .await
    .expect("blocking task");

    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    for (label, body) in bodies {
        assert_eq!(payload(&body)["scope"], "global", "{label}: {body}");
        assert_eq!(
            owner_of(&state, label),
            (
                ScopeKind::Global.as_str().to_string(),
                GLOBAL_SCOPE_OWNER_ID.to_string()
            ),
            "{label}"
        );
    }

    handle.shutdown().await;
}

/// The daemon's own catalog is unchanged by D-137: a `worktree` argument that
/// reaches the daemon (a proxy not opted in relays it untouched) is still an
/// unknown argument, rejected as before.
#[tokio::test]
async fn a_worktree_argument_reaching_the_daemon_is_still_rejected() {
    let (_home, layout) = open_layout();
    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let body = tokio::task::spawn_blocking(move || {
        Client::connect(&socket_path).call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall","arguments":{"worktree":"/repo"}}}"#,
            None,
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(
        body["error"]["code"],
        Value::Number((-32602).into()),
        "{body}"
    );
    handle.shutdown().await;
}
