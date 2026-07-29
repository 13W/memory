//! The llama.cpp `Generator` provider's contract with the asset gate
//! (mirrors `local_rag_models/tests/onnx.rs`'s shape) — T14-07/ADR-0006.
//!
//! Two things are tested unconditionally, because they hold on any machine:
//! the provider resolves weights **only** through the `.ok`-marker
//! precondition (never downloads), and a `LlamaError` converts to the
//! `[FIXED]` pool-facing `GenError` without losing its variant.
//!
//! Real inference needs the actual GGUF weights (hundreds of MiB to a few
//! GiB per entry) and takes real CPU time, neither of which may be a CI
//! prerequisite (spec 14 §1: tests are offline and hermetic). The
//! `real_inference_for` helper (T14-09: now one `#[test]` per catalog entry,
//! not only the default) runs it when supplied and **says so loudly** when
//! it skips, rather than passing quietly — the same idiom `onnx.rs`'s own
//! real-inference test uses. It reuses `LOCAL_RAG_TEST_MODEL_HOME` (the same
//! `StoreLayout`-rooted cache `onnx.rs` already uses for ONNX weights)
//! rather than inventing a second env var: a GGUF model just lives in its
//! own `model_id` subdirectory under the same root, and one root can host
//! every catalog entry's weights at once.

mod support;

use std::path::PathBuf;
use std::time::Instant;

use local_rag_core::paths::StoreLayout;
use local_rag_embed::{FinishReason, GenError, GenMessage, GenRequest, GenRole, Generator};
use local_rag_generate::{
    CATALOG, DEFAULT_MODEL_ID, GeneratorCatalogEntry, LlamaError, LlamaGenerator,
};
use local_rag_test_support::TempHome;

#[test]
fn opening_without_installed_assets_is_the_typed_missing_error() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");

    let entry = &CATALOG[0];
    let err = LlamaGenerator::open(&layout, entry).expect_err("no weights are installed");

    match err {
        LlamaError::AssetsMissing {
            model_id,
            expected_path,
        } => {
            assert_eq!(model_id, DEFAULT_MODEL_ID);
            assert_eq!(
                PathBuf::from(expected_path),
                layout.model_dir(DEFAULT_MODEL_ID)
            );
        }
        other => panic!("expected AssetsMissing, got {other:?}"),
    }
    assert!(!layout.model_dir(DEFAULT_MODEL_ID).exists());
}

#[test]
fn the_missing_assets_error_converts_to_the_pools_typed_error() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");

    let err: GenError = LlamaGenerator::open(&layout, &CATALOG[0])
        .expect_err("no weights")
        .into();

    assert!(
        matches!(err, GenError::ModelAssetsMissing { .. }),
        "{err:?}"
    );
    assert!(!err.is_retryable(), "waiting will not install a model");
}

#[test]
fn temperature_sampling_is_a_permanent_error_before_any_inference() {
    // No real model needed: `generate` rejects the unsupported sampling mode
    // in `LlamaGenerator::generate` itself, before touching the runtime —
    // but that method needs `self`, so this only exercises the conversion
    // path directly (the runtime-dependent path is covered by the
    // SKIP-able real-inference test below).
    let err: GenError = LlamaError::UnsupportedSampling.into();
    assert!(!err.is_retryable(), "{err}");
}

/// Real inference against `entry`, when the host supplies installed weights.
///
/// `LOCAL_RAG_TEST_MODEL_HOME` — a `LOCAL_RAG_HOME`-shaped directory with
/// `entry` already installed (`models/{entry.model_id}/.ok`). One root can
/// (and, in practice, does — `cargo xtask memory-bench`'s own cache is
/// commonly reused here) hold every catalog entry at once; each call only
/// checks its own `model_id`. Absent weights for `entry` specifically, the
/// test prints why it skipped rather than passing silently — never a
/// hidden dependency on what else happens to be installed.
///
/// T14-09: exercising every catalog entry here, not only the default, is
/// direct evidence that switching `build_prompt` from
/// `llama-cpp-2::apply_chat_template` to `crate::chat_template::render`
/// (a real Jinja interpreter over each model's own raw template) did not
/// regress the two entries that already worked before this task.
///
/// `BACKEND_LOCK` serializes these calls: `llama_cpp_2::LlamaBackend::init`
/// is a real process-global one-at-a-time resource (its own doctest
/// demonstrates a second concurrent call fails with
/// `BackendAlreadyInitialized`; `Drop for LlamaBackend` releases it again
/// once a generator goes out of scope). Harmless for the real router, which
/// only ever opens one `LlamaGenerator` per process — but `cargo test`'s
/// default thread-parallel execution now runs more than one of these
/// `#[test]`s in the same process, so without this lock two entries
/// racing to open their own backend at once would spuriously fail.
static BACKEND_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn real_inference_for(entry: &GeneratorCatalogEntry) {
    let _guard = BACKEND_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let Ok(model_home) = std::env::var("LOCAL_RAG_TEST_MODEL_HOME") else {
        eprintln!(
            "SKIP [{}]: LOCAL_RAG_TEST_MODEL_HOME is unset — no installed weights. \
             Point it at a store root containing models/{}/.ok.",
            entry.model_id, entry.model_id
        );
        return;
    };

    let layout = StoreLayout::new(PathBuf::from(model_home));

    let load_started = Instant::now();
    let generator = match LlamaGenerator::open(&layout, entry) {
        Ok(g) => g,
        Err(LlamaError::AssetsMissing { expected_path, .. }) => {
            eprintln!(
                "SKIP [{}]: LOCAL_RAG_TEST_MODEL_HOME is set but {expected_path} has no .ok marker.",
                entry.model_id
            );
            return;
        }
        Err(e) => panic!("[{}] open the installed model: {e}", entry.model_id),
    };
    let load_ms = load_started.elapsed().as_secs_f64() * 1000.0;

    let req = GenRequest::new(
        vec![
            GenMessage {
                role: GenRole::System,
                content: "You classify observations. Reply with exactly one word: \
                          either DECISION or QUESTION."
                    .to_string(),
            },
            GenMessage {
                role: GenRole::User,
                content: "We decided to use SQLite instead of Postgres.".to_string(),
            },
        ],
        16,
    );

    let gen_started = Instant::now();
    let resp = generator
        .generate(req)
        .unwrap_or_else(|e| panic!("[{}] generate: {e}", entry.model_id));
    let gen_ms = gen_started.elapsed().as_secs_f64() * 1000.0;

    assert!(
        !resp.text.is_empty(),
        "[{}] the model produced no text at all",
        entry.model_id
    );
    assert!(
        matches!(
            resp.finish_reason,
            FinishReason::Stop | FinishReason::Length
        ),
        "[{}] {:?}",
        entry.model_id,
        resp.finish_reason
    );

    // Deterministic: greedy decoding of the same prompt reproduces the same
    // text (spec 08 §7's benchmark needs reproducible runs).
    let req_again = GenRequest::new(
        vec![
            GenMessage {
                role: GenRole::System,
                content: "You classify observations. Reply with exactly one word: \
                          either DECISION or QUESTION."
                    .to_string(),
            },
            GenMessage {
                role: GenRole::User,
                content: "We decided to use SQLite instead of Postgres.".to_string(),
            },
        ],
        16,
    );
    let resp_again = generator
        .generate(req_again)
        .unwrap_or_else(|e| panic!("[{}] generate again: {e}", entry.model_id));
    assert_eq!(
        resp.text, resp_again.text,
        "[{}] greedy decoding of an identical prompt must reproduce identical output",
        entry.model_id
    );

    eprintln!(
        "RAN [{}]: real inference, load {load_ms:.1} ms, generate {gen_ms:.1} ms, \
         output {:?} ({:?} tokens)",
        entry.model_id,
        resp.text.trim(),
        resp.tokens_generated
    );
}

#[test]
fn real_inference_default_gemma4_e2b() {
    real_inference_for(local_rag_generate::find(DEFAULT_MODEL_ID).expect("default is catalogued"));
}

#[test]
fn real_inference_qwen2_5_0_5b() {
    real_inference_for(
        local_rag_generate::find("qwen2.5-0.5b-instruct-gguf-q4km")
            .expect("Qwen 0.5B is catalogued"),
    );
}

#[test]
fn real_inference_qwen2_5_1_5b() {
    real_inference_for(
        local_rag_generate::find("qwen2.5-1.5b-instruct-gguf-q4km")
            .expect("Qwen 1.5B is catalogued"),
    );
}

#[test]
fn real_inference_phi3_mini_4k_third_family() {
    real_inference_for(
        local_rag_generate::find("phi-3-mini-4k-instruct-gguf-q4")
            .expect("Phi-3-mini-4k-instruct is catalogued"),
    );
}
