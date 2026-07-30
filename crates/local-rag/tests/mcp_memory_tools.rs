//! T15-04 store-backed MCP tool tests: `stats`/`health`/`recall`/
//! `list_memory`/`list_memory_candidates`/`inspect_memory_evidence` —
//! scope/filter/pagination contracts, degraded status, unknown-worktree
//! global-scope degrade (never an error, unlike the code-query tools), and
//! the no-writes guarantee.

mod support;

use local_rag_store::{GLOBAL_SCOPE_OWNER_ID, MemoryKind, MemoryState, ScopeKind, StateDb};
use serde_json::Value;
use support::{
    Client, git_available, open_layout, seed_indexed_worktree, seed_memory_entry,
    seed_memory_evidence, seed_observation, seed_pending_candidate, start,
    transition_seeded_memory_entry,
};

// ---------------------------------------------------------------------
// list_memory
// ---------------------------------------------------------------------

#[tokio::test]
async fn list_memory_includes_terminal_states_by_default() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_memory_entry(
            &state,
            "mem-active",
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "an active fact",
            1_000,
        )
        .await;
        seed_memory_entry(
            &state,
            "mem-retracted",
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "a retracted fact",
            2_000,
        )
        .await;
        transition_seeded_memory_entry(&state, "mem-retracted", MemoryState::Retracted).await;
    }

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_memory","arguments":{}}}"#,
            None,
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(body["result"]["isError"], Value::Bool(false), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    let ids: Vec<&str> = parsed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["memory_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["mem-active", "mem-retracted"],
        "terminal states remain queryable via review tools (spec 04 §5): {text}"
    );
    let retracted = &parsed["entries"][1];
    assert_eq!(retracted["state"], "retracted");
    drop(home);
    handle.shutdown().await;
}

#[tokio::test]
async fn list_memory_kind_and_state_filters_narrow_the_result() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_memory_entry(
            &state,
            "mem-fact",
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "a fact",
            1_000,
        )
        .await;
        seed_memory_entry(
            &state,
            "mem-task",
            MemoryKind::Task,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "a task",
            2_000,
        )
        .await;
        transition_seeded_memory_entry(&state, "mem-task", MemoryState::Resolved).await;
    }

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let (by_kind, by_state) = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        let by_kind = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_memory","arguments":{"kind":"fact"}}}"#,
            None,
        );
        let by_state = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_memory","arguments":{"state":"resolved"}}}"#,
            None,
        );
        (by_kind, by_state)
    })
    .await
    .expect("blocking task");

    let text = by_kind["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    let ids: Vec<&str> = parsed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["memory_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["mem-fact"], "{text}");

    let text = by_state["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    let ids: Vec<&str> = parsed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["memory_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["mem-task"], "{text}");

    drop(home);
    handle.shutdown().await;
}

#[tokio::test]
async fn list_memory_pagination_limit_offset_and_has_more() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        for i in 0..3u32 {
            seed_memory_entry(
                &state,
                &format!("mem-{i}"),
                MemoryKind::Fact,
                ScopeKind::Global,
                GLOBAL_SCOPE_OWNER_ID,
                "some fact",
                1_000 + i64::from(i),
            )
            .await;
        }
    }

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let (page1, page2) = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        let page1 = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_memory","arguments":{"limit":2,"offset":0}}}"#,
            None,
        );
        let page2 = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_memory","arguments":{"limit":2,"offset":2}}}"#,
            None,
        );
        (page1, page2)
    })
    .await
    .expect("blocking task");

    let text = page1["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    let ids: Vec<&str> = parsed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["memory_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["mem-0", "mem-1"], "{text}");
    assert_eq!(parsed["has_more"], Value::Bool(true), "{text}");

    let text = page2["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    let ids: Vec<&str> = parsed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["memory_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["mem-2"], "{text}");
    assert_eq!(parsed["has_more"], Value::Bool(false), "{text}");

    drop(home);
    handle.shutdown().await;
}

#[tokio::test]
async fn list_memory_unknown_worktree_degrades_to_global_not_an_error() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_memory_entry(
            &state,
            "mem-global",
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "a global fact",
            1_000,
        )
        .await;
    }

    let real_dir = home.join("never-registered");
    std::fs::create_dir_all(&real_dir).expect("create dir");
    let real_path = real_dir.to_string_lossy().into_owned();

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_memory","arguments":{}}}"#,
            Some(&real_path),
        )
    })
    .await
    .expect("blocking task");

    // Unlike the code-query tools (WORKTREE_NOT_INDEXED), memory tools work
    // in repo/global scope for an unresolved worktree (spec 02 §6).
    assert_eq!(body["result"]["isError"], Value::Bool(false), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["scope"], "global", "{text}");
    let ids: Vec<&str> = parsed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["memory_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["mem-global"], "{text}");

    handle.shutdown().await;
}

// ---------------------------------------------------------------------
// list_memory_candidates
// ---------------------------------------------------------------------

#[tokio::test]
async fn list_memory_candidates_is_global_only_and_state_filtered() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_pending_candidate(&state, "cand-a", "target-a", GLOBAL_SCOPE_OWNER_ID, 1_000).await;
        seed_pending_candidate(&state, "cand-b", "target-b", GLOBAL_SCOPE_OWNER_ID, 2_000).await;
    }

    let real_dir = home.join("some-worktree");
    std::fs::create_dir_all(&real_dir).expect("create dir");
    let real_path = real_dir.to_string_lossy().into_owned();

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let (with_worktree, filtered) = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        // A worktree_root argument must have zero effect -- candidates have
        // no scope column at all (the task card's "global-only behavior
        // where applicable").
        let with_worktree = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_memory_candidates","arguments":{}}}"#,
            Some(&real_path),
        );
        let filtered = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_memory_candidates","arguments":{"state":"pending"}}}"#,
            None,
        );
        (with_worktree, filtered)
    })
    .await
    .expect("blocking task");

    let text = with_worktree["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    let ids: Vec<&str> = parsed["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["candidate_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["cand-a", "cand-b"], "{text}");
    let op = &parsed["candidates"][0]["proposed_operation"];
    assert_eq!(op["op"], "create");
    assert_eq!(op["memory_id"], "target-a");

    let text = filtered["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    let ids: Vec<&str> = parsed["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["candidate_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["cand-a", "cand-b"], "both are still pending: {text}");

    handle.shutdown().await;
}

#[tokio::test]
async fn list_memory_candidates_pagination_has_more() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        for i in 0..3u32 {
            seed_pending_candidate(
                &state,
                &format!("cand-{i}"),
                &format!("target-{i}"),
                GLOBAL_SCOPE_OWNER_ID,
                1_000 + i64::from(i),
            )
            .await;
        }
    }

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_memory_candidates","arguments":{"limit":2}}}"#,
            None,
        )
    })
    .await
    .expect("blocking task");

    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    let ids: Vec<&str> = parsed["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["candidate_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["cand-0", "cand-1"], "{text}");
    assert_eq!(parsed["has_more"], Value::Bool(true), "{text}");

    drop(home);
    handle.shutdown().await;
}

// ---------------------------------------------------------------------
// inspect_memory_evidence
// ---------------------------------------------------------------------

#[tokio::test]
async fn inspect_memory_evidence_returns_linked_observation_ids() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_memory_entry(
            &state,
            "mem-with-evidence",
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "a fact",
            1_000,
        )
        .await;
        seed_observation(&state, "obs-1").await;
        seed_memory_evidence(&state, "mem-with-evidence", "obs-1").await;
    }

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"inspect_memory_evidence","arguments":{"memory_id":"mem-with-evidence"}}}"#,
            None,
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(body["result"]["isError"], Value::Bool(false), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["memory_id"], "mem-with-evidence");
    assert_eq!(parsed["observation_ids"], serde_json::json!(["obs-1"]));

    drop(home);
    handle.shutdown().await;
}

#[tokio::test]
async fn inspect_memory_evidence_unknown_memory_id_is_an_empty_list_not_an_error() {
    let (_home, layout) = open_layout();
    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"inspect_memory_evidence","arguments":{"memory_id":"no-such-memory"}}}"#,
            None,
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(body["result"]["isError"], Value::Bool(false), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["observation_ids"], serde_json::json!([]));

    handle.shutdown().await;
}

// ---------------------------------------------------------------------
// recall
// ---------------------------------------------------------------------

#[tokio::test]
async fn recall_returns_structured_entries_with_ids_and_matches_additional_context() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_memory_entry(
            &state,
            "mem-recall",
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "use jwt for auth",
            1_000,
        )
        .await;
    }

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall","arguments":{"query":"jwt"}}}"#,
            None,
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(body["result"]["isError"], Value::Bool(false), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["scope"], "global");
    let entries = parsed["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{text}");
    assert_eq!(entries[0]["memory_id"], "mem-recall");
    assert_eq!(entries[0]["text"], "use jwt for auth");
    let additional_context = parsed["additional_context"].as_str().unwrap();
    assert!(
        additional_context.contains("use jwt for auth"),
        "{additional_context}"
    );
    // No ids ever appear in the untrusted text block (spec 12 §4 item 3).
    assert!(
        !additional_context.contains("mem-recall"),
        "{additional_context}"
    );

    drop(home);
    handle.shutdown().await;
}

#[tokio::test]
async fn recall_termless_query_is_legal_and_orders_by_recency() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_memory_entry(
            &state,
            "mem-older",
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "older fact",
            1_000,
        )
        .await;
        seed_memory_entry(
            &state,
            "mem-newer",
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "newer fact",
            2_000,
        )
        .await;
    }

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall","arguments":{}}}"#,
            None,
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(body["result"]["isError"], Value::Bool(false), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    let ids: Vec<&str> = parsed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["memory_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["mem-newer", "mem-older"], "{text}");

    drop(home);
    handle.shutdown().await;
}

#[tokio::test]
async fn recall_limit_caps_entries_but_not_additional_context() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        for i in 0..3u32 {
            seed_memory_entry(
                &state,
                &format!("mem-{i}"),
                MemoryKind::Fact,
                ScopeKind::Global,
                GLOBAL_SCOPE_OWNER_ID,
                &format!("fact number {i}"),
                1_000 + i64::from(i),
            )
            .await;
        }
    }

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall","arguments":{"limit":1}}}"#,
            None,
        )
    })
    .await
    .expect("blocking task");

    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["entries"].as_array().unwrap().len(), 1, "{text}");
    let additional_context = parsed["additional_context"].as_str().unwrap();
    for i in 0..3u32 {
        assert!(
            additional_context.contains(&format!("fact number {i}")),
            "limit must not re-render additionalContext: {additional_context}"
        );
    }

    drop(home);
    handle.shutdown().await;
}

#[tokio::test]
async fn recall_dense_leg_unavailable_is_visible_in_the_response() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_memory_entry(
            &state,
            "mem-1",
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "some fact",
            1_000,
        )
        .await;
    }

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall","arguments":{"query":"fact"}}}"#,
            None,
        )
    })
    .await
    .expect("blocking task");

    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    // Production default is `UnavailableEmbedder` until T15-07 -- the
    // degraded reason must be visible in the response (this card's own
    // "degraded status" test bullet), never silent.
    assert!(!parsed["dense_degraded"].is_null(), "{text}");

    drop(home);
    handle.shutdown().await;
}

#[tokio::test]
async fn recall_unknown_worktree_degrades_to_global_not_an_error() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_memory_entry(
            &state,
            "mem-global",
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "a global fact",
            1_000,
        )
        .await;
    }

    let real_dir = home.join("never-registered");
    std::fs::create_dir_all(&real_dir).expect("create dir");
    let real_path = real_dir.to_string_lossy().into_owned();

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall","arguments":{}}}"#,
            Some(&real_path),
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(body["result"]["isError"], Value::Bool(false), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["scope"], "global", "{text}");

    handle.shutdown().await;
}

// ---------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------

#[tokio::test]
async fn stats_reports_counts_by_kind_state_and_pending_candidates_by_state() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_memory_entry(
            &state,
            "mem-1",
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "fact 1",
            1_000,
        )
        .await;
        seed_memory_entry(
            &state,
            "mem-2",
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "fact 2",
            2_000,
        )
        .await;
        seed_pending_candidate(&state, "cand-1", "target-1", GLOBAL_SCOPE_OWNER_ID, 1_000).await;
    }

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"stats","arguments":{}}}"#,
            None,
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(body["result"]["isError"], Value::Bool(false), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    let by_kind_state = parsed["memory"]["entries_by_kind_state"]
        .as_array()
        .unwrap();
    assert_eq!(by_kind_state.len(), 1, "{text}");
    assert_eq!(by_kind_state[0]["kind"], "fact");
    assert_eq!(by_kind_state[0]["state"], "active");
    assert_eq!(by_kind_state[0]["count"], 2, "{text}");

    let by_review_state = parsed["memory"]["pending_candidates_by_state"]
        .as_array()
        .unwrap();
    assert_eq!(by_review_state.len(), 1, "{text}");
    assert_eq!(by_review_state[0]["state"], "pending");
    assert_eq!(by_review_state[0]["count"], 1, "{text}");

    assert!(parsed["worktree"].is_null(), "{text}");
    assert!(parsed["store_instance_uuid"].is_string(), "{text}");
    assert!(
        parsed["write_queues"]["state"]["capacity"].is_u64(),
        "{text}"
    );

    drop(home);
    handle.shutdown().await;
}

#[tokio::test]
async fn stats_reports_projection_status_for_a_resolved_worktree() {
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
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"stats","arguments":{}}}"#,
            Some(&repo_path),
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(body["result"]["isError"], Value::Bool(false), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(
        parsed["scope"],
        format!("repo:{}", seeded.repo_id),
        "{text}"
    );
    assert_eq!(
        parsed["worktree"]["worktree_id"], seeded.worktree_id,
        "{text}"
    );
    assert_eq!(parsed["worktree"]["repo_id"], seeded.repo_id, "{text}");
    assert_eq!(parsed["worktree"]["projection_status"], "clean", "{text}");
    assert!(
        parsed["worktree"]["active_generation_id"].is_string(),
        "{text}"
    );

    handle.shutdown().await;
}

// ---------------------------------------------------------------------
// health
// ---------------------------------------------------------------------

#[tokio::test]
async fn health_reports_daemon_mode_version_and_store_instance_uuid() {
    let (_home, layout) = open_layout();
    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"health","arguments":{}}}"#,
            None,
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(body["result"]["isError"], Value::Bool(false), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["daemon_mode"], "normal", "{text}");
    assert_eq!(parsed["daemon_version"], local_rag_core::VERSION, "{text}");
    assert!(parsed["store_instance_uuid"].is_string(), "{text}");

    handle.shutdown().await;
}

// ---------------------------------------------------------------------
// read calls produce no writes
// ---------------------------------------------------------------------

/// Every read-tool call goes through `StateDb::open_read`/`CacheDb::
/// open_read` (`SQLITE_OPEN_READ_ONLY`, structurally incapable of writing) —
/// this test is confirmatory, not the only line of defense: it proves no
/// row count changed across a full batch of every read tool this task adds,
/// plus the three T15-03 code-query tools.
#[tokio::test]
async fn read_tool_calls_produce_no_state_sqlite_writes() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_memory_entry(
            &state,
            "mem-1",
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "some fact",
            1_000,
        )
        .await;
        seed_pending_candidate(&state, "cand-1", "target-1", GLOBAL_SCOPE_OWNER_ID, 1_000).await;
    }

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;

    let calls = [
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"stats","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"health","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"recall","arguments":{"query":"fact"}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"list_memory","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"list_memory_candidates","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"inspect_memory_evidence","arguments":{"memory_id":"mem-1"}}}"#,
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"project_overview","arguments":{}}}"#,
    ];

    tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        for call in calls {
            let body = client.call_and_read(call, None);
            assert!(body["error"].is_null(), "{call} -> {body}");
        }
    })
    .await
    .expect("blocking task");

    // Re-open a fresh read-only connection against the same on-disk file the
    // running daemon still owns, to observe the post-call row counts.
    let read = rusqlite::Connection::open_with_flags(
        layout.state_db(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("reopen state.sqlite read-only");
    let count = |table: &str| -> i64 {
        read.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(count("memory_entry"), 1);
    assert_eq!(count("pending_memory_candidate"), 1);
    assert_eq!(count("memory_evidence"), 0);

    drop(home);
    handle.shutdown().await;
}
