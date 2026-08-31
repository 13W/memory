//! T20-07 acceptance tests for the `admin/projects_list`/`admin/
//! projects_reload`/`admin/reconcile_now` JSON-RPC verbs — end to end
//! through a real `DaemonHandle` and the real MCP wire protocol. Contract
//! snapshots and the `MigrationOnly` `available: false` shape are also unit
//! -tested directly in `daemon::mcp::dispatch`'s own `#[cfg(test)]` module
//! (no store/socket needed there); this file is the one place that proves
//! the whole chain — dispatch → `SupervisorClient` → the supervisor actor →
//! `WorktreeTaskHandle::trigger` — actually works together. T20-06's own
//! registration/reload/shutdown semantics are covered by
//! `tests/indexing_supervisor.rs` and are not re-proven here.

#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use local_rag::daemon::{DaemonHandle, LazyEmbedderProvider, ProviderProbe};
use local_rag::indexing::register_new_worktree;
use local_rag_core::identity::domain::path_fingerprint;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_embed::{Embedder, HashingEmbedder};
use local_rag_store::{
    DEFAULT_MODEL_SPACE_ID, RepresentationKind, StateDb, WorktreeKind, WorktreeRootFacts,
    projection_state, register_managed_worktree, register_representation, set_managed_enabled,
    set_model_space_representation,
};
use local_rag_test_support::TempHome;
use support::{Client, open_layout, start_options};

/// Every test here spins up a real `DaemonHandle` (real socket, real
/// dedicated OS threads per worktree task) — same self-contention risk under
/// libtest's default parallel-within-one-binary scheduling that
/// `tests/indexing_supervisor.rs` already documents and fixes the same way.
static SEQUENTIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn serialize_heavy_daemon_tests() -> tokio::sync::MutexGuard<'static, ()> {
    SEQUENTIAL.lock().await
}

struct SeqUuids {
    counter: AtomicU64,
}

impl SeqUuids {
    fn new() -> Self {
        SeqUuids {
            counter: AtomicU64::new(0),
        }
    }
}

impl UuidSource for SeqUuids {
    fn next_uuid(&self) -> Uuid {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        uuidv7_from(9_910_000 + n, [0x58; 10])
    }
}

fn facts_for(root: &Path) -> WorktreeRootFacts {
    let path = root.display().to_string();
    WorktreeRootFacts {
        observed_canonical_path: path.clone(),
        display_path: path.clone(),
        path_fingerprint: path_fingerprint(&path),
        kind: WorktreeKind::NonGit,
        common_dir_fingerprint: None,
        remote_fingerprint: None,
    }
}

async fn register_representations(state: &StateDb, now_ms: i64) {
    let code_key = HashingEmbedder::new(RepresentationKind::CodeRaw).key();
    let memory_key = HashingEmbedder::new(RepresentationKind::Memory).key();
    state
        .writer()
        .transaction(move |tx| {
            let code_id = register_representation(tx, "test-code-raw", &code_key, now_ms)?;
            set_model_space_representation(
                tx,
                DEFAULT_MODEL_SPACE_ID,
                RepresentationKind::CodeRaw,
                &code_id,
                true,
                now_ms,
            )?;
            let memory_id = register_representation(tx, "test-memory", &memory_key, now_ms)?;
            set_model_space_representation(
                tx,
                DEFAULT_MODEL_SPACE_ID,
                RepresentationKind::Memory,
                &memory_id,
                true,
                now_ms,
            )
        })
        .await
        .expect("register representations");
}

async fn seed_managed_worktree(
    home: &TempHome,
    state: &StateDb,
    uuids: &SeqUuids,
    dir_name: &str,
    enabled: bool,
    now_ms: i64,
) -> (Uuid, PathBuf) {
    let root = home.join(dir_name);
    std::fs::create_dir_all(&root).expect("create worktree root");
    std::fs::write(root.join("main.rs"), "fn parse_config() {}\n").expect("seed file");

    let repo_id = uuids.next_uuid();
    let worktree_id = uuids.next_uuid();
    let facts = facts_for(&root);
    register_new_worktree(state, repo_id, worktree_id, &facts, now_ms)
        .await
        .expect("register worktree");

    let worktree_id_str = worktree_id.to_string();
    state
        .writer()
        .transaction(move |tx| {
            register_managed_worktree(tx, &worktree_id_str, now_ms)?;
            if !enabled {
                set_managed_enabled(tx, &worktree_id_str, false, now_ms)?;
            }
            Ok(())
        })
        .await
        .expect("enroll managed worktree");

    (worktree_id, root)
}

fn ready_embedder_provider() -> Arc<LazyEmbedderProvider> {
    Arc::new(LazyEmbedderProvider::with_probes(
        || ProviderProbe::Ready(Arc::new(HashingEmbedder::new(RepresentationKind::CodeRaw))),
        || ProviderProbe::Ready(Arc::new(HashingEmbedder::new(RepresentationKind::Memory))),
    ))
}

/// Every call site here bounds a real cold-start `spawn_watcher` (`notify::
/// recommended_watcher(...)?.watch(root, RecursiveMode::Recursive)?`,
/// `crates/index/src/reconcile/watcher.rs:68-98`) — a synchronous OS call
/// into the platform FSEvents/inotify backend, not application logic. On
/// this repo's shared dev machine that call was directly measured (T20-07
/// diagnosis, temporary `eprintln!` instrumentation, reverted) taking up to
/// ~90s under real FSEvents-subsystem backlog before returning — never
/// hanging, always eventually completing. 120s keeps real margin over that
/// observed worst case without masking an actual hang.
async fn wait_for(deadline: Duration, mut check: impl FnMut() -> bool) {
    tokio::time::timeout(deadline, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition did not become true within the bound");
}

fn active_generation(state: &StateDb, worktree_id: &Uuid) -> Option<String> {
    let conn = state.open_read().expect("open a read connection");
    projection_state(&conn, &worktree_id.to_string())
        .expect("read projection state")
        .and_then(|row| row.active_generation_id)
}

#[tokio::test]
async fn admin_projects_list_joins_durable_and_live_status_end_to_end() {
    let _guard = serialize_heavy_daemon_tests().await;
    let (home, layout) = open_layout();
    let uuids = SeqUuids::new();
    let now_ms = 1_000;

    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    register_representations(&state, now_ms).await;
    let (enabled_id, _) = seed_managed_worktree(&home, &state, &uuids, "on", true, now_ms).await;
    let (disabled_id, _) = seed_managed_worktree(&home, &state, &uuids, "off", false, now_ms).await;
    drop(state);

    let mut opts = start_options(layout.clone());
    opts.embedder_provider = ready_embedder_provider();
    let socket_path = layout.socket_path();
    let handle = DaemonHandle::start(opts).await.expect("start");

    let state = StateDb::open(layout.state_db()).expect("reopen state.sqlite for polling");
    wait_for(Duration::from_secs(120), || {
        active_generation(&state, &enabled_id).is_some()
    })
    .await;

    let (enabled_id_str, disabled_id_str) = (enabled_id.to_string(), disabled_id.to_string());
    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"admin/projects_list"}"#,
            None,
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(body["result"]["available"], true, "{body}");
    let projects = body["result"]["projects"]
        .as_array()
        .expect("projects is an array");
    assert_eq!(projects.len(), 2, "{body}");

    let enabled_entry = projects
        .iter()
        .find(|p| p["worktree_id"] == enabled_id_str)
        .expect("the enabled row is present");
    assert_eq!(enabled_entry["enabled"], true);
    assert!(
        enabled_entry["task"]["last_generation_id"].is_string(),
        "{enabled_entry}"
    );

    let disabled_entry = projects
        .iter()
        .find(|p| p["worktree_id"] == disabled_id_str)
        .expect("the disabled row is present");
    assert_eq!(disabled_entry["enabled"], false);
    assert!(
        disabled_entry["task"].is_null(),
        "a disabled row must never have a live task: {disabled_entry}"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn admin_reconcile_now_on_an_unregistered_worktree_is_a_typed_json_rpc_error() {
    let _guard = serialize_heavy_daemon_tests().await;
    let (_home, layout) = open_layout();
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    register_representations(&state, 1_000).await;
    drop(state);

    let mut opts = start_options(layout.clone());
    opts.embedder_provider = ready_embedder_provider();
    let socket_path = layout.socket_path();
    let handle = DaemonHandle::start(opts).await.expect("start");

    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"admin/reconcile_now","params":{"worktree_id":"ghost"}}"#,
            None,
        )
    })
    .await
    .expect("blocking task");

    assert_eq!(body["error"]["code"], -32602, "{body}");
    assert!(body.get("result").is_none(), "{body}");

    handle.shutdown().await;
}

#[tokio::test]
async fn admin_reconcile_now_forces_a_new_generation() {
    let _guard = serialize_heavy_daemon_tests().await;
    let (home, layout) = open_layout();
    let uuids = SeqUuids::new();
    let now_ms = 1_000;

    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    register_representations(&state, now_ms).await;
    let (worktree_id, root) =
        seed_managed_worktree(&home, &state, &uuids, "repo", true, now_ms).await;
    drop(state);

    let mut opts = start_options(layout.clone());
    opts.embedder_provider = ready_embedder_provider();
    let socket_path = layout.socket_path();
    let handle = DaemonHandle::start(opts).await.expect("start");

    let state = StateDb::open(layout.state_db()).expect("reopen state.sqlite for polling");
    wait_for(Duration::from_secs(120), || {
        active_generation(&state, &worktree_id).is_some()
    })
    .await;
    let before = active_generation(&state, &worktree_id);

    std::fs::write(root.join("main.rs"), "fn parse_config() {}\nfn two() {}\n")
        .expect("modify seed file");

    let worktree_id_str = worktree_id.to_string();
    let socket_path_for_call = socket_path.clone();
    let body = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path_for_call);
        let params = serde_json::json!({ "worktree_id": worktree_id_str }).to_string();
        client.call_and_read(
            &format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"admin/reconcile_now","params":{params}}}"#
            ),
            None,
        )
    })
    .await
    .expect("blocking task");
    assert_eq!(body["result"]["available"], true, "{body}");

    wait_for(Duration::from_secs(120), || {
        let current = active_generation(&state, &worktree_id);
        current.is_some() && current != before
    })
    .await;

    handle.shutdown().await;
}

#[tokio::test]
async fn admin_verbs_answer_available_false_in_migration_only() {
    let _guard = serialize_heavy_daemon_tests().await;
    let (_home, layout) = open_layout();

    // Mirrors `tests/lifecycle_startup.rs`'s own incompatible-store fixture:
    // migrate to latest, then hand-insert a `schema_migrations` row this
    // binary's own `ALL` set does not know about.
    {
        let mut conn = rusqlite::Connection::open(layout.state_db()).expect("open state db");
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
            rusqlite::params![max + 1, 600],
        )
        .expect("seed a from-the-future migration row");
    }

    let mut opts = start_options(layout.clone());
    opts.embedder_provider = ready_embedder_provider();
    let socket_path = layout.socket_path();
    let handle = DaemonHandle::start(opts)
        .await
        .expect("start must still succeed (degraded mode, not a hard failure)");
    assert!(
        handle.indexing_supervisor().is_none(),
        "no supervisor in MigrationOnly"
    );

    let (list, reload, reconcile) = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        let list = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"admin/projects_list"}"#,
            None,
        );
        let reload = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":2,"method":"admin/projects_reload"}"#,
            None,
        );
        let reconcile = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":3,"method":"admin/reconcile_now","params":{"worktree_id":"any"}}"#,
            None,
        );
        (list, reload, reconcile)
    })
    .await
    .expect("blocking task");

    assert_eq!(list["result"]["available"], false, "{list}");
    assert_eq!(list["result"]["projects"], serde_json::json!([]), "{list}");
    assert_eq!(reload["result"]["available"], false, "{reload}");
    assert_eq!(reconcile["result"]["available"], false, "{reconcile}");

    handle.shutdown().await;
}
