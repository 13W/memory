//! Offline smoke: the whole embedding path with no network, no daemon, no
//! weights, and no `$HOME`.
//!
//! Spec 10 §1 `[FIXED]` requires the local backend to be "the working default"
//! and Ollama/remote to be "strictly optional"; spec 01 §1 `[FIXED]` allows "no
//! mandatory external daemons". This file proves that operationally (a full
//! embed → cache round trip inside a `TempHome`) and structurally (the crate's
//! manifest declares no network/model SDK at all, so there is nothing that
//! *could* dial out).
//!
//! It also pins the deterministic model fixture: golden components, stability
//! across instances and threads, and the cache round trip through the
//! little-endian codec the store already owns.

mod support;

use std::sync::Arc;
use std::thread;

use local_rag_core::config::DataPolicy;
use local_rag_core::identity::domain::subject_content_blob;
use local_rag_core::paths::StoreLayout;
use local_rag_embed::{
    EmbedError, EmbedRequest, Embedder, HashingEmbedder, LOCAL_BOOTSTRAP_MODEL_ID, ProviderEntry,
    ProviderPool, model_assets_dir, require_model_assets,
};
use local_rag_store::{
    CacheDb, EmbeddingKey, RepresentationKind, StateDb, SubjectKind, decode_vector_le,
    encode_vector_le, get_embedding, insert_embedding, register_representation,
    verify_cached_embedding,
};
use local_rag_test_support::TempHome;
use support::batch;

const NOW_MS: i64 = 1_700_000_000_000;
const REPRESENTATION_ID: &str = "44444444-4444-7444-8444-444444444444";
const STORE_UUID: &str = "55555555-5555-7555-8555-555555555555";

/// Embed a batch and land it in `cache.sqlite`, entirely offline.
#[tokio::test(flavor = "multi_thread")]
async fn embed_and_cache_round_trip_is_fully_local() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let cache = CacheDb::open(layout.cache_db(), STORE_UUID).expect("open cache.sqlite");

    let embedder = Arc::new(HashingEmbedder::new(RepresentationKind::CodeRaw));
    let key = embedder.key();
    let dimensions = key.dimensions;

    let registered = key.clone();
    state
        .writer()
        .transaction(move |tx| register_representation(tx, REPRESENTATION_ID, &registered, NOW_MS))
        .await
        .expect("register representation");

    let pool = ProviderPool::new(vec![ProviderEntry::local("local", embedder.clone())]);
    let texts = batch();
    let vectors = pool
        .embed(
            DataPolicy::LocalOnly,
            EmbedRequest::new(RepresentationKind::CodeRaw, texts.clone()),
        )
        .expect("embed offline");

    // Write each vector under its own subject hash, exactly as T11-04's backfill
    // will (spec 03 §1.2/§4.2).
    let keys: Vec<EmbeddingKey> = texts
        .iter()
        .map(|text| EmbeddingKey {
            subject_kind: SubjectKind::ContentBlob,
            subject_hash: subject_content_blob(&format!("blob:{text}")),
            representation_id: REPRESENTATION_ID.to_string(),
        })
        .collect();

    let to_write: Vec<(EmbeddingKey, Vec<f32>)> = keys
        .iter()
        .cloned()
        .zip(vectors.iter().map(|v| v.as_slice().to_vec()))
        .collect();
    cache
        .writer()
        .transaction(move |tx| {
            for (key, vector) in &to_write {
                insert_embedding(tx, key, i64::from(dimensions), vector, NOW_MS)?;
            }
            Ok(())
        })
        .await
        .expect("write embeddings");

    let conn = cache.open_read().expect("read cache");
    for (key, expected) in keys.iter().zip(&vectors) {
        let row = get_embedding(&conn, key)
            .expect("read embedding")
            .expect("row present");
        verify_cached_embedding(&row).expect("cached row is intact");
        assert_eq!(row.dimensions, i64::from(dimensions));
        assert_eq!(
            row.byte_size,
            encode_vector_le(expected.as_slice()).len() as i64
        );
        assert_eq!(
            decode_vector_le(&row.vector_f32).expect("decode"),
            expected.as_slice(),
            "the cached vector must be byte-identical to the embedded one"
        );
    }
}

/// The fixture embedder is byte-deterministic: same text, same components —
/// across instances, across threads, across runs.
#[test]
fn the_model_fixture_is_byte_deterministic() {
    let text = "fn parse(input: &str) -> Result<Ast, Error>";
    let reference = HashingEmbedder::new(RepresentationKind::CodeRaw).embed_one(text);

    // A second instance produces the identical vector.
    assert_eq!(
        HashingEmbedder::new(RepresentationKind::CodeRaw).embed_one(text),
        reference
    );

    // So do concurrent threads (no shared mutable state, no global seed).
    let handles: Vec<_> = (0..4)
        .map(|_| {
            thread::spawn(move || HashingEmbedder::new(RepresentationKind::CodeRaw).embed_one(text))
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().expect("thread"), reference);
    }

    // Golden components (captured from a real run, not hand-written): a drift in
    // the hashing algorithm would silently invalidate every cached vector under
    // this representation, and fails here instead.
    let nonzero: Vec<(usize, f32)> = reference
        .as_slice()
        .iter()
        .enumerate()
        .filter(|(_, v)| **v != 0.0)
        .map(|(i, v)| (i, *v))
        .collect();
    assert_eq!(
        nonzero.len(),
        13,
        "unexpected sparsity pattern: {nonzero:?}"
    );
    assert_eq!(nonzero[0].0, 49);
    assert!((nonzero[0].1 - (-0.2773501)).abs() < 1e-6, "{nonzero:?}");
    assert_eq!(
        nonzero.last().copied().map(|(i, _)| i),
        Some(245),
        "{nonzero:?}"
    );
    let norm = reference
        .as_slice()
        .iter()
        .map(|x| x * x)
        .sum::<f32>()
        .sqrt();
    assert!((norm - 1.0).abs() < 1e-6, "unit length, got {norm}");
}

/// The crate cannot dial out: its manifest declares no network client, no model
/// runtime and no external-daemon SDK. A structural guard, not a promise —
/// adding `reqwest`/`ort`/`fastembed`/an Ollama client here fails this test.
#[test]
fn the_crate_declares_no_network_or_model_dependency() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("read crates/embed/Cargo.toml");

    // Only the `[dependencies]` section is load-bearing: dev-dependencies never
    // link into the shipped binary.
    let deps = manifest
        .split("[dependencies]")
        .nth(1)
        .expect("a [dependencies] section")
        .split("\n[")
        .next()
        .expect("section body");

    for forbidden in [
        "reqwest",
        "hyper",
        "ureq",
        "curl",
        "isahc",
        "surf",
        "tonic",
        "rustls",
        "openssl",
        "native-tls",
        "ollama",
        "fastembed",
        "ort",
        "onnxruntime",
        "candle",
        "llama",
        "tokenizers",
        "hf-hub",
    ] {
        assert!(
            !deps.contains(forbidden),
            "crates/embed must not depend on `{forbidden}`: T11-03 ships no network client and no \
             model runtime (spec 10 §1's local default; the ONNX provider and its weights are \
             T11-06, see D-008)"
        );
    }

    // Positively: the only dependencies are this workspace's own crates.
    for line in deps.lines().filter(|l| l.contains('=')) {
        assert!(
            line.trim_start().starts_with("local-rag-"),
            "unexpected external dependency in crates/embed: {line}"
        );
    }
}

/// Model assets are a *typed precondition*, not an implicit download: with no
/// `.ok` marker the local provider path fails with `ModelAssetsMissing`, and no
/// network call is attempted (there is nothing here that could make one).
#[test]
fn missing_model_assets_are_a_typed_error_not_a_download() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");

    let err = require_model_assets(&layout, "some-onnx-model").expect_err("no assets installed");
    match &err {
        EmbedError::ModelAssetsMissing {
            model_id,
            expected_path,
        } => {
            assert_eq!(model_id, "some-onnx-model");
            assert!(
                expected_path.ends_with("models/some-onnx-model"),
                "{expected_path}"
            );
        }
        other => panic!("expected ModelAssetsMissing, got {other}"),
    }

    // A half-installed directory (no `.ok` marker) is still "missing": spec 10
    // §5's atomic install is `.part → fsync → rename → .ok`.
    let dir = model_assets_dir(&layout, "some-onnx-model");
    std::fs::create_dir_all(&dir).expect("create model dir");
    std::fs::write(dir.join("model.onnx.part"), b"partial").expect("write partial");
    assert!(require_model_assets(&layout, "some-onnx-model").is_err());

    // Once the marker lands, the directory is usable.
    std::fs::write(dir.join(".ok"), b"").expect("write marker");
    assert_eq!(
        require_model_assets(&layout, "some-onnx-model").expect("assets present"),
        dir
    );

    // The bootstrap embedder needs no assets at all — that is why it can be the
    // working default today.
    assert!(
        HashingEmbedder::new(RepresentationKind::CodeRaw)
            .embed(EmbedRequest::new(
                RepresentationKind::CodeRaw,
                vec!["fn main() {}".to_string()]
            ))
            .is_ok(),
        "the bootstrap provider must work with no installed model ({LOCAL_BOOTSTRAP_MODEL_ID})"
    );
}
