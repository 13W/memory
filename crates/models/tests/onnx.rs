//! The ONNX provider's contract with the asset gate (D-008, spec 10 §1/§5) —
//! T11-06.
//!
//! Two things are tested unconditionally, because they hold on any machine:
//!
//! * the provider resolves weights **only** through
//!   `local_rag_embed::require_model_assets`, so a missing model is the typed
//!   `ModelAssetsMissing` and never an implicit download;
//! * its representation key is exactly ADR-0004's, field for field — the key is
//!   what makes a cached vector reusable (spec 03 §2.2), so a drift here would
//!   silently invalidate the whole embedding cache.
//!
//! Real inference needs the ONNX Runtime shared library and ~295 MB of weights,
//! neither of which may be a CI prerequisite (spec 14 §1: tests are offline and
//! hermetic). [`real_inference_when_the_runtime_and_weights_are_present`] runs it
//! when both are supplied and **says so loudly** when it skips, rather than
//! passing quietly. The recorded local run is in `PROGRESS.md`'s T11-06 evidence.

mod support;

use std::path::PathBuf;
use std::time::Instant;

use local_rag_core::paths::StoreLayout;
use local_rag_embed::{EmbedError, EmbedRequest, Embedder, verify_registered_key};
use local_rag_models::{
    DEFAULT_MODEL_ID, EMBEDDINGGEMMA_300M, HttpFetcher, OnnxEmbedder, OnnxError, install_model,
};
use local_rag_store::{
    DistanceMetric, RepresentationKind, StateDb, register_representation, representation_key,
};
use local_rag_test_support::TempHome;
use support::{FIXTURE_MODEL_ID, FixtureServer};

/// Fixed ids/clock: nothing here may depend on wall time or a random id.
const REPRESENTATION_ID: &str = "66666666-6666-7666-8666-666666666666";
const NOW_MS: i64 = 1_700_000_000_000;

#[test]
fn opening_without_installed_assets_is_the_typed_missing_error() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");

    let err = OnnxEmbedder::open(&layout, &EMBEDDINGGEMMA_300M)
        .err()
        .expect("no weights are installed");

    match err {
        OnnxError::Assets(EmbedError::ModelAssetsMissing {
            model_id,
            expected_path,
        }) => {
            assert_eq!(model_id, DEFAULT_MODEL_ID);
            assert_eq!(
                PathBuf::from(expected_path),
                layout.model_dir(DEFAULT_MODEL_ID),
                "the error points at the directory the installer would fill"
            );
        }
        other => panic!("expected ModelAssetsMissing, got {other:?}"),
    }

    // Nothing was created as a side effect: a provider never installs.
    assert!(!layout.model_dir(DEFAULT_MODEL_ID).exists());
}

#[test]
fn the_missing_assets_error_converts_to_the_pools_typed_error() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");

    let err: EmbedError = OnnxEmbedder::open(&layout, &EMBEDDINGGEMMA_300M)
        .err()
        .expect("no weights")
        .into();

    // T11-04 classifies `ModelAssetsMissing` as fatal for a backfill run; the
    // conversion must preserve the variant rather than flatten it to a string.
    assert!(
        matches!(err, EmbedError::ModelAssetsMissing { .. }),
        "{err:?}"
    );
    assert!(!err.is_retryable(), "waiting will not install a model");
}

#[test]
fn a_marked_directory_gets_past_the_gate_and_fails_on_the_asset_itself() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let server = FixtureServer::start();
    let entry = server.entry();

    // The fixture "model" is installed correctly — marker and all — but its
    // `tokenizer.json` is not a real tokenizer. The distinction matters: the
    // provider must fail on the *contents* here, not on the marker check, which
    // is what proves the gate was passed rather than skipped.
    install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink()).expect("install");

    let err = OnnxEmbedder::open(&layout, &entry)
        .err()
        .expect("the fixture is not a real model");
    assert!(
        matches!(err, OnnxError::Tokenizer(_)),
        "expected a tokenizer failure, got {err:?}"
    );
    assert!(
        err.to_string().contains("tokenizer"),
        "the message names what failed: {err}"
    );
    // The install itself is intact; only loading failed.
    assert!(
        layout.model_dir(FIXTURE_MODEL_ID).join(".ok").is_file(),
        "a failed load never revokes an installation"
    );
}

#[test]
fn the_representation_key_is_exactly_the_one_adr_0004_fixed() {
    let key = EMBEDDINGGEMMA_300M.representation_key();

    // ADR-0004's four decided fields — model, width, metric, kind — are frozen.
    assert_eq!(key.kind, RepresentationKind::CodeRaw);
    assert_eq!(key.model_id, "embeddinggemma-300m");
    assert_eq!(key.dimensions, 768);
    assert_eq!(key.distance_metric, DistanceMetric::Cosine);
    assert_eq!(key.normalization_version, 1);

    // `representation_version` is the deliberate exception: it moves whenever
    // something outside the other five fields changes the vectors. D-016 raised
    // `MAX_SEQUENCE_TOKENS` 256 → 1024 (version 2); D-017 moved the provider onto
    // the graph's pooled output, i.e. through the model's Dense head instead of
    // around it (version 3). Vectors from either earlier era must stop being
    // addressable under this key.
    assert_eq!(key.representation_version, 3);
    assert_eq!(local_rag_models::MAX_SEQUENCE_TOKENS, 1024);
}

/// Real inference, when the host supplies both halves.
///
/// * `ORT_DYLIB_PATH` — the ONNX Runtime shared library (`ort`'s `load-dynamic`
///   reads this variable itself);
/// * `LOCAL_RAG_TEST_MODEL_HOME` — a `LOCAL_RAG_HOME`-shaped directory with the
///   default model already installed (`models/embeddinggemma-300m/.ok`).
///
/// Absent either, the test prints why it skipped. A silent pass would let the
/// provider rot undetected between here and T17-03's platform matrix.
#[tokio::test(flavor = "multi_thread")]
async fn real_inference_when_the_runtime_and_weights_are_present() {
    let Ok(dylib) = std::env::var("ORT_DYLIB_PATH") else {
        eprintln!(
            "SKIP: ORT_DYLIB_PATH is unset — no ONNX Runtime to load. \
             Set it to libonnxruntime.{{so,dylib,dll}} to run this test."
        );
        return;
    };
    let Ok(model_home) = std::env::var("LOCAL_RAG_TEST_MODEL_HOME") else {
        eprintln!(
            "SKIP: LOCAL_RAG_TEST_MODEL_HOME is unset — no installed weights. \
             Point it at a store root containing models/{DEFAULT_MODEL_ID}/.ok."
        );
        return;
    };
    assert!(
        PathBuf::from(&dylib).exists(),
        "ORT_DYLIB_PATH points at {dylib}, which does not exist"
    );

    let layout = StoreLayout::new(PathBuf::from(model_home));
    let load_started = Instant::now();
    let embedder = OnnxEmbedder::open(&layout, &EMBEDDINGGEMMA_300M)
        .unwrap_or_else(|e| panic!("open the installed model: {e}"));
    let load_ms = load_started.elapsed().as_secs_f64() * 1000.0;

    let key = embedder.key();
    assert_eq!(key, EMBEDDINGGEMMA_300M.representation_key());

    // D-017: the installed graph declares `last_hidden_state` first and
    // `sentence_embedding` second, and only the second one runs the model's
    // trained Dense head. Selecting by position embedded into a space the model
    // never produces, and nothing downstream could tell — both outputs are
    // 768-wide and normalize cleanly. This assertion is the detector.
    assert_eq!(
        embedder.output_name(),
        local_rag_models::POOLED_OUTPUT,
        "the provider must embed through the graph's pooled output, not the raw token states"
    );

    // The card's contract: `key()` and the vector width must agree with the
    // *registered* representation, checked through the very code the pool uses
    // (T11-03's `verify_registered_key`), not by comparing two constants.
    let home = TempHome::new().expect("temp home");
    let state_layout = StoreLayout::new(home.join("local-rag"));
    state_layout.ensure().expect("ensure store tree");
    let state = StateDb::open(state_layout.state_db()).expect("open state.sqlite");
    let registered_key = key.clone();
    let representation_id = state
        .writer()
        .transaction(move |tx| {
            register_representation(tx, REPRESENTATION_ID, &registered_key, NOW_MS)
        })
        .await
        .expect("register representation");
    let conn = state.open_read().expect("read connection");
    assert_eq!(
        verify_registered_key(&conn, &representation_id, &embedder.key()).expect("verify"),
        Ok(()),
        "the provider must match the representation its vectors are cached under"
    );

    let texts = vec![
        "fn parse_config(path: &Path) -> Result<Config, ConfigError>".to_string(),
        "def parse_config(path): return Config.load(path)".to_string(),
        "SELECT id FROM users WHERE email = ?".to_string(),
    ];
    let embed_started = Instant::now();
    let vectors = embedder
        .embed(EmbedRequest {
            kind: RepresentationKind::CodeRaw,
            texts: texts.clone(),
        })
        .expect("embed");
    let embed_ms = embed_started.elapsed().as_secs_f64() * 1000.0;

    assert_eq!(vectors.len(), texts.len());
    let registered_dimensions = representation_key(&conn, &representation_id)
        .expect("read back the registered key")
        .expect("the row exists")
        .dimensions;
    for vector in &vectors {
        assert_eq!(
            vector.as_slice().len(),
            registered_dimensions as usize,
            "the vector width must equal the registered representation"
        );
        let norm: f32 = vector.as_slice().iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-3,
            "cosine distance assumes unit vectors, got |v| = {norm}"
        );
        assert!(vector.as_slice().iter().all(|v| v.is_finite()));
    }

    // The two config parsers should sit closer together than either does to the
    // SQL statement — a coarse but real check that this is a semantic model and
    // not, say, a zero tensor.
    let cosine = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
    let same_topic = cosine(vectors[0].as_slice(), vectors[1].as_slice());
    let different_topic = cosine(vectors[0].as_slice(), vectors[2].as_slice());
    assert!(
        same_topic > different_topic,
        "expected the two config parsers to be closer: {same_topic} vs {different_topic}"
    );

    // Deterministic: the same text embeds to the same vector.
    let again = embedder
        .embed(EmbedRequest {
            kind: RepresentationKind::CodeRaw,
            texts: vec![texts[0].clone()],
        })
        .expect("embed again");
    for (a, b) in vectors[0].as_slice().iter().zip(again[0].as_slice()) {
        assert!((a - b).abs() < 1e-5, "{a} vs {b}");
    }

    // A provider that silently served the wrong representation kind would poison
    // the cache under a key it does not own; the refusal is typed and permanent.
    let wrong = embedder
        .embed(EmbedRequest {
            kind: RepresentationKind::CodeContext,
            texts: vec![texts[0].clone()],
        })
        .expect_err("a provider serves exactly one representation kind");
    assert!(!wrong.is_retryable(), "{wrong}");

    // An empty batch is not an inference call at all.
    assert!(
        embedder
            .embed(EmbedRequest {
                kind: RepresentationKind::CodeRaw,
                texts: Vec::new(),
            })
            .expect("empty batch")
            .is_empty()
    );

    eprintln!(
        "RAN: real inference over {} texts, {} dimensions, load {load_ms:.1} ms, \
         batch {embed_ms:.1} ms ({:.1} ms/text), cos(same topic)={same_topic:.3} \
         > cos(different)={different_topic:.3}, dylib {dylib}",
        texts.len(),
        key.dimensions,
        embed_ms / texts.len() as f64
    );
}
