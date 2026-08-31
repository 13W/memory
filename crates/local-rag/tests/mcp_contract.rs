//! T15-03 contract tests: the real MCP JSON-RPC dispatcher over a real
//! `DaemonHandle` + UDS connection — `initialize`/notifications/`tools/
//! list`, malformed JSON-RPC, unknown method/tool/params, `id` preservation,
//! `MigrationOnly` behavior.

#![cfg(unix)]

mod support;

use local_rag::daemon::{DaemonMode, MigrationOnlyReason};
use serde_json::Value;
use support::{Client, open_layout, start};

#[tokio::test]
async fn initialize_returns_server_identity_and_instructions() {
    let (_home, layout) = open_layout();
    let socket_path = layout.socket_path();
    let handle = start(&layout).await;

    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            None,
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(body["result"]["serverInfo"]["name"], "local-rag");
    assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
    assert!(
        body["result"]["instructions"]
            .as_str()
            .expect("instructions is a string")
            .contains("search_code"),
        "{body}"
    );
    handle.shutdown().await;
}

/// The load-bearing test for `RequestHandler::handle`'s `Option` return:
/// after a notification, the *next line read* must be the next real
/// request's response, never a stray line the notification produced. A
/// bare timeout would pass vacuously if the daemon silently answered the
/// notification too — this formulation is the only one that actually
/// catches that.
#[tokio::test]
async fn a_notification_produces_no_response_line() {
    let (_home, layout) = open_layout();
    let socket_path = layout.socket_path();
    let handle = start(&layout).await;

    let (id, is_tools_list) = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            None,
        );
        let body = client.call_and_read(r#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#, None);
        (body["id"].clone(), body["result"]["tools"].is_array())
    })
    .await
    .expect("blocking task");

    assert_eq!(id, Value::Number(9.into()));
    assert!(
        is_tools_list,
        "the next line must be tools/list's own response"
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn tools_list_advertises_all_nineteen_v0_tools() {
    let (_home, layout) = open_layout();
    let socket_path = layout.socket_path();
    let handle = start(&layout).await;

    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, None)
    })
    .await
    .expect("blocking task");

    let names: Vec<&str> = body["result"]["tools"]
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
    // is true for exactly the two entry-terminating ones (retract_memory and,
    // since D-079, reject_memory -- `rejected` drops a hypothesis out of
    // recall the same way `retracted` does for the other kinds).
    let destructive: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| {
            assert!(
                t["annotations"].is_object(),
                "{} has no annotations",
                t["name"]
            );
            (t["annotations"]["destructiveHint"] == Value::Bool(true))
                .then(|| t["name"].as_str().unwrap())
        })
        .collect();
    assert_eq!(destructive, ["retract_memory", "reject_memory"]);

    // v1 name mapping (spec 11 §2): forget -> retract_memory,
    // consolidate(src,tgt) -> merge_memories -- neither v1 name is ever
    // exposed as its own tool (this task card's own test bullet).
    assert!(!names.contains(&"forget"), "{names:?}");
    assert!(!names.contains(&"consolidate"), "{names:?}");
    handle.shutdown().await;
}

#[tokio::test]
async fn malformed_json_rpc_envelopes_get_invalid_request() {
    let (_home, layout) = open_layout();
    let socket_path = layout.socket_path();
    let handle = start(&layout).await;

    let results = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        let cases = [
            "[1,2,3]",                                     // top-level array
            "\"just a string\"",                           // top-level scalar
            r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#, // wrong jsonrpc
            r#"{"jsonrpc":"2.0","id":1}"#,                 // missing method
        ];
        cases
            .iter()
            .map(|c| client.call_and_read(c, None)["error"]["code"].clone())
            .collect::<Vec<_>>()
    })
    .await
    .expect("blocking task");

    for code in results {
        assert_eq!(code, Value::Number((-32600).into()));
    }
    handle.shutdown().await;
}

#[tokio::test]
async fn an_unknown_method_gets_method_not_found() {
    let (_home, layout) = open_layout();
    let socket_path = layout.socket_path();
    let handle = start(&layout).await;

    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(r#"{"jsonrpc":"2.0","id":1,"method":"bogus/method"}"#, None)
    })
    .await
    .expect("blocking task");

    assert_eq!(body["error"]["code"], Value::Number((-32601).into()));
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bogus/method")
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn tools_call_argument_errors_get_invalid_params() {
    let (_home, layout) = open_layout();
    let socket_path = layout.socket_path();
    let handle = start(&layout).await;

    let results = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        let cases = [
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_code","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_code","arguments":{"query":"x","limit":0}}}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"search_code","arguments":{"query":"x","mode":"graph"}}}"#,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search_code","arguments":{"query":"x","bogus":true}}}"#,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"inspect_memory_evidence","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"recall","arguments":{"limit":0}}}"#,
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"list_memory","arguments":{"kind":"bogus"}}}"#,
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"list_memory","arguments":{"state":"bogus"}}}"#,
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"list_memory_candidates","arguments":{"state":"bogus"}}}"#,
            r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"list_memory","arguments":{"offset":-1}}}"#,
            r#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"stats","arguments":{"bogus":true}}}"#,
            r#"{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"health","arguments":{"bogus":true}}}"#,
            r#"{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"remember","arguments":{"text":"x"}}}"#,
            r#"{"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":"remember","arguments":{"text":"x","kind":"bogus"}}}"#,
            r#"{"jsonrpc":"2.0","id":16,"method":"tools/call","params":{"name":"remember","arguments":{"text":"","kind":"fact"}}}"#,
            r#"{"jsonrpc":"2.0","id":17,"method":"tools/call","params":{"name":"approve_memory_candidate","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":18,"method":"tools/call","params":{"name":"reject_memory_candidate","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":19,"method":"tools/call","params":{"name":"edit_memory_candidate","arguments":{"id":"c-1"}}}"#,
            r#"{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"edit_memory_candidate","arguments":{"id":"c-1","patch":{"bogus":true}}}}"#,
            r#"{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"edit_memory","arguments":{"id":"m-1","patch":{"text":"x"}}}}"#,
            r#"{"jsonrpc":"2.0","id":22,"method":"tools/call","params":{"name":"edit_memory","arguments":{"id":"m-1","expected_version":1,"patch":{}}}}"#,
            r#"{"jsonrpc":"2.0","id":23,"method":"tools/call","params":{"name":"retract_memory","arguments":{"id":"m-1"}}}"#,
            r#"{"jsonrpc":"2.0","id":24,"method":"tools/call","params":{"name":"merge_memories","arguments":{"ids":[{"memory_id":"m-1","expected_version":1}],"survivor_id":"m-1"}}}"#,
            r#"{"jsonrpc":"2.0","id":25,"method":"tools/call","params":{"name":"merge_memories","arguments":{"ids":[{"memory_id":"m-1","expected_version":1},{"memory_id":"m-2","expected_version":1}],"survivor_id":"m-3"}}}"#,
            r#"{"jsonrpc":"2.0","id":26,"method":"tools/call","params":{"name":"give_feedback","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":27,"method":"tools/call","params":{"name":"get_file_context","arguments":{}}}"#,
        ];
        cases
            .iter()
            .map(|c| client.call_and_read(c, None)["error"]["code"].clone())
            .collect::<Vec<_>>()
    })
    .await
    .expect("blocking task");

    for code in results {
        assert_eq!(code, Value::Number((-32602).into()));
    }
    handle.shutdown().await;
}

#[tokio::test]
async fn the_id_is_preserved_for_string_number_and_null() {
    let (_home, layout) = open_layout();
    let socket_path = layout.socket_path();
    let handle = start(&layout).await;

    let ids = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        let a = client.call_and_read(r#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#, None)["id"]
            .clone();
        let b = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":123456789012,"method":"ping"}"#,
            None,
        )["id"]
            .clone();
        let c = client.call_and_read(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#, None)["id"]
            .clone();
        (a, b, c)
    })
    .await
    .expect("blocking task");

    assert_eq!(ids.0, Value::String("abc".to_string()));
    assert_eq!(ids.1, Value::Number(123456789012i64.into()));
    assert_eq!(ids.2, Value::Null);
    handle.shutdown().await;
}

#[tokio::test]
async fn migration_only_serves_the_handshake_but_refuses_tool_calls() {
    let (_home, layout) = open_layout();

    // Same fixture `tests/lifecycle_startup.rs::a_checksum_drift_store_enters_migration_only_mode_too`
    // uses: migrate to latest, then corrupt one applied migration's checksum.
    {
        let mut conn = rusqlite::Connection::open(layout.state_db()).expect("open state db");
        local_rag_store::migrate::run(
            &mut conn,
            local_rag_store::ALL,
            &layout.migration_lock(),
            500,
        )
        .expect("migrate to latest");
        conn.execute(
            "UPDATE schema_migrations SET checksum = 'corrupted' WHERE version = 1",
            [],
        )
        .expect("corrupt version 1's checksum");
    }

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    assert!(matches!(
        &*handle.mode.borrow(),
        DaemonMode::MigrationOnly {
            reason: MigrationOnlyReason::ChecksumDrift { .. }
        }
    ));

    let (initialize_ok, tools_list_ok, call_body, health_body) =
        tokio::task::spawn_blocking(move || {
            let mut client = Client::connect(&socket_path);
            let initialize =
                client.call_and_read(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#, None);
            let tools_list =
                client.call_and_read(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, None);
            let call = client.call_and_read(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"project_overview","arguments":{}}}"#,
                None,
            );
            // A T15-04 tool must be refused identically -- no per-tool
            // exception to the MigrationOnly gate, including for
            // "health"/"stats" (see `mcp::memory::health`'s own doc comment
            // for why that is not a gap: migration status is diagnosable
            // earlier, over the handshake/CLI channels).
            let health = client.call_and_read(
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"health","arguments":{}}}"#,
                None,
            );
            (
                initialize["result"]["serverInfo"]["name"] == "local-rag",
                tools_list["result"]["tools"].is_array(),
                call,
                health,
            )
        })
        .await
        .expect("blocking task");

    assert!(initialize_ok, "initialize must work even in MigrationOnly");
    assert!(tools_list_ok, "tools/list must work even in MigrationOnly");

    let content_text = call_body["result"]["content"][0]["text"]
        .as_str()
        .expect("content text");
    assert_eq!(call_body["result"]["isError"], Value::Bool(true));
    assert!(
        content_text.contains("\"code\":\"INCOMPATIBLE_STORE\""),
        "{content_text}"
    );

    let health_text = health_body["result"]["content"][0]["text"]
        .as_str()
        .expect("content text");
    assert_eq!(health_body["result"]["isError"], Value::Bool(true));
    assert!(
        health_text.contains("\"code\":\"INCOMPATIBLE_STORE\""),
        "{health_text}"
    );

    handle.shutdown().await;
}
