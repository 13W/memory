//! D-037: a model installed *after* the daemon started must be picked up by
//! the next `search_code`/`recall`, with no `local-rag restart`.
//!
//! Both dense-leg providers used to be resolved exactly once, while `serve`
//! assembled `StartOptions`, so a daemon that came up before `local-rag init
//! --download-models` stayed `lexical_only`/`dense_degraded` for its whole
//! lifetime. `daemon::query_embedder::LazyQueryEmbedder` re-probes instead.
//!
//! The probe is the seam under test, not ONNX: these tests hand it a marker
//! file to watch and a fixed-vector provider to hand back, so the transition
//! is exercised end to end through a real live daemon and the real MCP wire
//! protocol with no weights, no ONNX Runtime and no inference — exactly the
//! split `crates/models/tests/install.rs` (installer) and
//! `tests/offline_search_recall.rs`'s `with_real_model` module (real
//! inference) already cover from their own ends.

mod support;

use std::path::PathBuf;
use std::sync::Arc;

use local_rag::daemon::{DaemonHandle, LazyQueryEmbedder, ProviderProbe};
use local_rag_store::{
    DEFAULT_MODEL_SPACE_ID, DistanceMetric, GLOBAL_SCOPE_OWNER_ID, MemoryKind, RepresentationKey,
    RepresentationKind, ScopeKind, StateDb, register_representation,
    set_model_space_representation,
};
use serde_json::Value;
use support::{
    Client, git_available, open_layout, seed_indexed_worktree, seed_memory_entry, start_options,
};

/// A provider answering the fixture's own 3-wide vector — the one
/// `support::seed_indexed_worktree` projects every occurrence to, and the one
/// the memory representations below are registered at. Stands in for "a real
/// provider opened once the weights landed" without any of the weights.
struct FixtureEmbedder;

impl local_rag_search::QueryEmbedder for FixtureEmbedder {
    fn embed_query(
        &self,
        _query: &str,
        _key: &RepresentationKey,
    ) -> Result<Vec<f32>, local_rag_search::QueryEmbedError> {
        Ok(vec![1.0, 0.0, 0.0])
    }
}

impl local_rag_memory::recall::QueryEmbedder for FixtureEmbedder {
    fn embed_query(
        &self,
        _query: &str,
        _key: &RepresentationKey,
    ) -> Result<Vec<f32>, local_rag_memory::recall::QueryEmbedError> {
        Ok(vec![1.0, 0.0, 0.0])
    }
}

/// `code_query_embedder`'s shape with the `.ok`-marker check pointed at
/// `marker` and `OnnxEmbedder::open` replaced by [`FixtureEmbedder`].
fn lazy_code_embedder(marker: PathBuf) -> Arc<dyn local_rag_search::QueryEmbedder> {
    let unavailable: Arc<dyn local_rag_search::QueryEmbedder> =
        Arc::new(local_rag_search::UnavailableEmbedder);
    Arc::new(LazyQueryEmbedder::new(unavailable, move || {
        if marker.is_file() {
            ProviderProbe::Ready(Arc::new(FixtureEmbedder))
        } else {
            ProviderProbe::NotInstalled
        }
    }))
}

/// [`lazy_code_embedder`]'s memory-side twin.
fn lazy_memory_embedder(marker: PathBuf) -> Arc<dyn local_rag_memory::recall::QueryEmbedder> {
    let unavailable: Arc<dyn local_rag_memory::recall::QueryEmbedder> =
        Arc::new(local_rag_memory::recall::UnavailableEmbedder);
    Arc::new(LazyQueryEmbedder::new(unavailable, move || {
        if marker.is_file() {
            ProviderProbe::Ready(Arc::new(FixtureEmbedder))
        } else {
            ProviderProbe::NotInstalled
        }
    }))
}

fn tool_call(id: u32, name: &str, arguments: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
    )
}

fn payload(body: &Value) -> Value {
    assert_eq!(body["result"]["isError"], Value::Bool(false), "{body}");
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("tool result carries a text block");
    serde_json::from_str(text).expect("tool result text is JSON")
}

#[tokio::test]
async fn search_code_leaves_lexical_only_once_the_model_lands_without_a_restart() {
    if !git_available() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (home, layout) = open_layout();
    let seeded = seed_indexed_worktree(&home, &layout).await;

    // Nothing installed yet: the marker this daemon's probe watches does not
    // exist when it starts, exactly like a store `local-rag init
    // --download-models` has not been run against.
    let marker = home.join("model-installed.ok");
    assert!(!marker.exists());

    let mut opts = start_options(layout.clone());
    opts.query_embedder = Some(lazy_code_embedder(marker.clone()));
    let socket_path = layout.socket_path();
    let handle = DaemonHandle::start(opts).await.expect("start");

    let repo_path = seeded.repo_path.to_string_lossy().into_owned();
    let before = {
        let socket_path = socket_path.clone();
        let repo_path = repo_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut client = Client::connect(&socket_path);
            client.call_and_read(
                &tool_call(1, "search_code", r#"{"query":"hello"}"#),
                Some(&repo_path),
            )
        })
        .await
        .expect("blocking task")
    };
    let before = payload(&before);
    assert_eq!(
        before["degraded"],
        Value::String("lexical_only".to_string()),
        "no model installed yet: {before}"
    );

    // The operator runs `local-rag init --download-models` against the live
    // daemon's store. No restart, no signal, no new connection state.
    std::fs::write(&marker, b"").expect("install the model");

    let after = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(
            &tool_call(2, "search_code", r#"{"query":"hello","mode":"code"}"#),
            Some(&repo_path),
        )
    })
    .await
    .expect("blocking task");
    let after = payload(&after);
    assert_eq!(
        after["degraded"],
        Value::Null,
        "the dense-only `code` mode must serve once the model is installed, \
         without a restart: {after}"
    );
    assert!(
        !after["results"]
            .as_array()
            .expect("results is an array")
            .is_empty(),
        "the dense leg must actually return the seeded occurrence: {after}"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn recall_leaves_its_degraded_dense_leg_once_the_model_lands_without_a_restart() {
    let (home, layout) = open_layout();

    // A registered `memory` representation is what makes the *embedder* the
    // deciding factor: without one, recall reports `no_representation` and
    // never calls a provider at all, so the transition would be invisible.
    // Mirrors `tests/mcp_memory_tools.rs`'s own dense-recall fixture.
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_memory_entry(
            &state,
            "mem-1",
            MemoryKind::Fact,
            ScopeKind::Global,
            GLOBAL_SCOPE_OWNER_ID,
            "a distinctive memory fact",
            1_000,
        )
        .await;
        state
            .writer()
            .transaction(move |tx| {
                let id = register_representation(
                    tx,
                    "mem-repr-1",
                    &RepresentationKey {
                        kind: RepresentationKind::Memory,
                        representation_version: 1,
                        normalization_version: 1,
                        model_id: "fixture-model".to_string(),
                        dimensions: 3,
                        distance_metric: DistanceMetric::Cosine,
                    },
                    1_000,
                )?;
                set_model_space_representation(
                    tx,
                    DEFAULT_MODEL_SPACE_ID,
                    RepresentationKind::Memory,
                    &id,
                    true,
                    1_000,
                )?;
                Ok(())
            })
            .await
            .expect("register memory representation");
    }

    let marker = home.join("model-installed.ok");
    let mut opts = start_options(layout.clone());
    opts.memory_query_embedder = Some(lazy_memory_embedder(marker.clone()));
    let socket_path = layout.socket_path();
    let handle = DaemonHandle::start(opts).await.expect("start");

    let before = {
        let socket_path = socket_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut client = Client::connect(&socket_path);
            client.call_and_read(&tool_call(1, "recall", r#"{"query":"distinctive"}"#), None)
        })
        .await
        .expect("blocking task")
    };
    let before = payload(&before);
    assert_eq!(
        before["dense_degraded"],
        Value::String("embed_failed: no query embedder wired".to_string()),
        "no model installed yet: {before}"
    );

    std::fs::write(&marker, b"").expect("install the model");

    let after = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        client.call_and_read(&tool_call(2, "recall", r#"{"query":"distinctive"}"#), None)
    })
    .await
    .expect("blocking task");
    let after = payload(&after);
    assert_eq!(
        after["dense_degraded"],
        Value::Null,
        "recall's dense leg must serve once the model is installed, without a \
         restart: {after}"
    );

    drop(home);
    handle.shutdown().await;
}
