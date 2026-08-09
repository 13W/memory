//! T17-04 acceptance test: "offline search/recall" (spec 13 §2 `[FIXED
//! list]`: "fully offline operation after `local-rag init --download-models`").
//!
//! `crates/models/tests/install.rs` already proves the *installer* contract
//! (`reading_installed_assets_never_reaches_the_fetcher`) and
//! `crates/embed/tests/offline_smoke.rs` proves no network-capable dependency
//! exists anywhere in the embedding path at all — this file proves the layer
//! neither covers: a **live daemon**, driven over the real MCP wire protocol,
//! actually answers `search_code`/`remember`/`recall` correctly with no
//! model installed at all (tier A, always on) and with a real installed
//! model whose fetch source is long gone (tier B, opt-in, mirrors
//! `tests/cli_index.rs`'s own `with_real_model` convention).

mod support;

use serde_json::Value;
use support::{Client, git_available, open_layout, seed_indexed_worktree, start};

/// No model installed at all: `search_code`'s dense leg degrades to
/// `lexical_only` (`main.rs::build_query_embedder`'s documented fallback),
/// yet the lexical leg alone still finds the seeded occurrence, and a
/// `remember`/`recall` round trip works purely through the local op engine
/// and FTS/brute-force-cosine recall (spec 09 §1, spec 15 §2's MVP scope) —
/// nothing here could reach a network even if it tried:
/// `crates/embed/tests/offline_smoke.rs`'s manifest lint already proves no
/// network-capable dependency exists anywhere in this build's embedding
/// path, and `UnavailableEmbedder` (the default when no model is installed)
/// does not call out to anything at all.
#[tokio::test]
async fn search_code_and_remember_recall_succeed_fully_offline() {
    if !git_available() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (home, layout) = open_layout();
    let seeded = seed_indexed_worktree(&home, &layout).await;

    let socket_path = layout.socket_path();
    let handle = start(&layout).await;
    let repo_path = seeded.repo_path.to_string_lossy().into_owned();

    let (search_body, remember_body, recall_body) = tokio::task::spawn_blocking(move || {
        let mut client = Client::connect(&socket_path);
        let search_body = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_code","arguments":{"query":"hello"}}}"#,
            Some(&repo_path),
        );
        let remember_body = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"remember","arguments":{"text":"the offline test remembered this fact","kind":"fact"}}}"#,
            None,
        );
        let recall_body = client.call_and_read(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"recall","arguments":{}}}"#,
            None,
        );
        (search_body, remember_body, recall_body)
    })
    .await
    .expect("blocking task");

    assert_eq!(
        search_body["result"]["isError"],
        Value::Bool(false),
        "{search_body}"
    );
    let search_text = search_body["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let search_parsed: Value = serde_json::from_str(search_text).unwrap();
    assert_eq!(
        search_parsed["degraded"],
        Value::String("lexical_only".to_string()),
        "no model installed must degrade to lexical_only, never silently: {search_text}"
    );
    let results = search_parsed["results"]
        .as_array()
        .expect("results is an array");
    assert!(
        !results.is_empty(),
        "the lexical leg alone must still find the seeded occurrence: {search_text}"
    );
    assert_eq!(results[0]["path"], "src/lib.rs", "{search_text}");

    assert_eq!(
        remember_body["result"]["isError"],
        Value::Bool(false),
        "{remember_body}"
    );

    assert_eq!(
        recall_body["result"]["isError"],
        Value::Bool(false),
        "{recall_body}"
    );
    let recall_text = recall_body["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let recall_parsed: Value = serde_json::from_str(recall_text).unwrap();
    let additional_context = recall_parsed["additional_context"].as_str().unwrap();
    assert!(
        additional_context.contains("the offline test remembered this fact"),
        "{additional_context}"
    );

    handle.shutdown().await;
}

/// Real ONNX inference through a **live daemon MCP call** — unlike
/// `tests/cli_index.rs`'s/`tests/cli_doctor.rs`'s own real-model tests,
/// which only ever exercise real inference at `index`/`rebuild` time through
/// the CLI, this drives `search_code` itself against a running daemon whose
/// query embedder is a real `OnnxEmbedder`, after the fixture install server
/// (if any) is long gone — proving the daemon never needs it again once the
/// weights are on disk (`crates/models/tests/install.rs`'s own
/// `reading_installed_assets_never_reaches_the_fetcher`, one layer up).
///
/// Env-gated exactly like `tests/cli_index.rs`'s own `with_real_model`
/// module: skips loudly when `ORT_DYLIB_PATH`/`LOCAL_RAG_TEST_MODEL_HOME`
/// are not both set locally.
mod with_real_model {
    use std::path::{Path, PathBuf};
    use std::process::{Output, Stdio};
    use std::sync::Arc;

    use serde_json::Value;

    use local_rag::daemon::{DaemonHandle, EmbedderQueryAdapter};
    use local_rag_core::paths::StoreLayout;
    use local_rag_models::{DEFAULT_MODEL_ID, OnnxEmbedder, find, is_installed};
    use local_rag_search::QueryEmbedder;
    use local_rag_test_support::TempHome;

    use crate::support::{Client, open_layout, start_options};

    fn require_env() -> Option<(String, String)> {
        let dylib = std::env::var("ORT_DYLIB_PATH").ok();
        let model_home = std::env::var("LOCAL_RAG_TEST_MODEL_HOME").ok();
        match (dylib, model_home) {
            (Some(d), Some(m)) => Some((d, m)),
            _ => {
                eprintln!(
                    "SKIP: ORT_DYLIB_PATH and/or LOCAL_RAG_TEST_MODEL_HOME are unset — \
                     set both to run the real-model offline search/recall test."
                );
                None
            }
        }
    }

    /// Install the real default model into `layout` by symlinking it out of
    /// `LOCAL_RAG_TEST_MODEL_HOME` (never copied: the fixture weights are
    /// ~295 MB and this only runs when explicitly opted into locally).
    fn install_real_model(layout: &StoreLayout, model_home: &str) {
        let src = PathBuf::from(model_home)
            .join("models")
            .join(DEFAULT_MODEL_ID);
        assert!(
            src.join(".ok").is_file(),
            "{}: LOCAL_RAG_TEST_MODEL_HOME must already have {DEFAULT_MODEL_ID} installed",
            src.display()
        );
        let dst = layout.model_dir(DEFAULT_MODEL_ID);
        std::fs::create_dir_all(dst.parent().expect("models dir has a parent"))
            .expect("create models/ parent");
        std::os::unix::fs::symlink(&src, &dst).expect("symlink installed model");
    }

    /// `http_proxy`/`https_proxy` point at a port nothing listens on — a
    /// cheap, deterministic tripwire: if any future code path on this
    /// command's line ever attempted an HTTP fetch, it would fail fast and
    /// loudly rather than silently succeeding or hanging, backing up the
    /// "offline" claim with more than an absence of assertion.
    fn run_cli_with_ort(home: &TempHome, dir: &Path, dylib: &str, args: &[&str]) -> Output {
        let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
        cmd.args(args);
        cmd.current_dir(dir);
        cmd.env("ORT_DYLIB_PATH", dylib);
        cmd.env("http_proxy", "http://127.0.0.1:1");
        cmd.env("https_proxy", "http://127.0.0.1:1");
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.output().expect("run local-rag")
    }

    #[tokio::test]
    async fn real_dense_search_and_recall_work_once_the_fetch_server_is_gone() {
        let Some((dylib, model_home)) = require_env() else {
            return;
        };
        let (home, layout) = open_layout();
        install_real_model(&layout, &model_home);

        let init = run_cli_with_ort(&home, home.path(), &dylib, &["init"]);
        assert_eq!(init.status.code(), Some(0), "{init:?}");

        let target = home.join("project");
        std::fs::create_dir_all(&target).expect("create target dir");
        // A relevant file (retry/backoff) and an unrelated decoy
        // (temperature conversion), zero lexical overlap with the query
        // below — a top hit on the relevant file only happens if real
        // semantic (dense) matching actually ran.
        std::fs::write(
            target.join("retry.rs"),
            "fn retry_with_backoff(mut attempt: u32) {\n    while attempt < 5 {\n        \
             std::thread::sleep(std::time::Duration::from_millis(100 * attempt as u64));\n        \
             attempt += 1;\n    }\n}\n",
        )
        .expect("seed relevant file");
        std::fs::write(
            target.join("temperature.rs"),
            "fn celsius_to_fahrenheit(c: f64) -> f64 {\n    c * 9.0 / 5.0 + 32.0\n}\n",
        )
        .expect("seed decoy file");

        let index = run_cli_with_ort(
            &home,
            home.path(),
            &dylib,
            &["index", target.to_str().unwrap()],
        );
        assert_eq!(index.status.code(), Some(0), "{index:?}");

        // A real, live, in-process daemon whose dense leg is a real ONNX
        // provider — `main.rs::build_query_embedder`'s own construction,
        // replicated here since it is a private function of that binary.
        let entry = find(DEFAULT_MODEL_ID).expect("default model entry is known");
        assert!(
            is_installed(&layout, entry.model_id),
            "the real model must be installed on disk by now"
        );
        let embedder = OnnxEmbedder::open(&layout, entry).expect("open the real onnx embedder");
        let query_embedder: Arc<dyn QueryEmbedder> =
            Arc::new(EmbedderQueryAdapter::new(Arc::new(embedder)));

        let mut opts = start_options(layout.clone());
        opts.query_embedder = Some(query_embedder);
        let socket_path = layout.socket_path();
        let handle = DaemonHandle::start(opts)
            .await
            .expect("start a real daemon with a real embedder");

        let target_path = target.to_string_lossy().into_owned();
        let (search_body, remember_body, recall_body) = tokio::task::spawn_blocking(move || {
            let mut client = Client::connect(&socket_path);
            let search_body = client.call_and_read(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_code","arguments":{"query":"wait longer between each failed network attempt","mode":"code"}}}"#,
                Some(&target_path),
            );
            let remember_body = client.call_and_read(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"remember","arguments":{"text":"the real-model offline test remembered this fact","kind":"fact"}}}"#,
                None,
            );
            let recall_body = client.call_and_read(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"recall","arguments":{}}}"#,
                None,
            );
            (search_body, remember_body, recall_body)
        })
        .await
        .expect("blocking task");

        assert_eq!(
            search_body["result"]["isError"],
            Value::Bool(false),
            "{search_body}"
        );
        let search_text = search_body["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        let search_parsed: Value = serde_json::from_str(search_text).unwrap();
        assert_eq!(
            search_parsed["degraded"],
            Value::Null,
            "a real embedder must serve the dense-only `code` leg without degrading: {search_text}"
        );
        let results = search_parsed["results"]
            .as_array()
            .expect("results is an array");
        assert!(!results.is_empty(), "{search_text}");
        assert!(
            results[0]["path"].as_str().unwrap().contains("retry.rs"),
            "the semantically related file must rank first, proving real dense \
             inference ran through the live MCP call, not just at index time: {search_text}"
        );

        assert_eq!(
            remember_body["result"]["isError"],
            Value::Bool(false),
            "{remember_body}"
        );
        assert_eq!(
            recall_body["result"]["isError"],
            Value::Bool(false),
            "{recall_body}"
        );
        let recall_text = recall_body["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        let recall_parsed: Value = serde_json::from_str(recall_text).unwrap();
        let additional_context = recall_parsed["additional_context"].as_str().unwrap();
        assert!(
            additional_context.contains("the real-model offline test remembered this fact"),
            "{additional_context}"
        );

        handle.shutdown().await;
    }
}
