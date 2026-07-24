//! A provider's `key()` against the representation registry (spec 03 §2.2, 10 §2).
//!
//! `embedding_cache` rows carry a `representation_id` and never inline model
//! params `[FIXED]`, so the only thing standing between a swapped provider and
//! silently corrupt cache rows is this check. Every assertion below runs against
//! a real `state.sqlite` in a `TempHome` (migration 6's `representation` table),
//! not a hand-built mock.

mod support;

use std::sync::Arc;

use local_rag_core::config::DataPolicy;
use local_rag_core::paths::StoreLayout;
use local_rag_embed::{
    EmbedError, EmbedRequest, Embedder, HashingEmbedder, ProviderEntry, ProviderPool,
    RegistryMismatch, Vector, register_embedder_representation, verify_registered_key,
};
use local_rag_store::{
    DistanceMetric, RepresentationKey, RepresentationKind, StateDb, encode_vector_le,
};
use local_rag_test_support::TempHome;
use support::batch;

const NOW_MS: i64 = 1_700_000_000_000;
const REPRESENTATION_ID: &str = "22222222-2222-7222-8222-222222222222";

fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// Register a provider's key and read it back: the round trip must be exact,
/// and re-registering must converge on the same id.
#[tokio::test(flavor = "multi_thread")]
async fn a_providers_key_registers_and_verifies() {
    let (_home, db) = open_state();
    let embedder = Arc::new(HashingEmbedder::new(RepresentationKind::CodeRaw));

    let for_tx = embedder.clone();
    let id = db
        .writer()
        .transaction(move |tx| {
            register_embedder_representation(tx, REPRESENTATION_ID, for_tx.as_ref(), NOW_MS)
        })
        .await
        .expect("register representation");
    assert_eq!(id, REPRESENTATION_ID);

    let conn = db.open_read().expect("read connection");
    assert_eq!(
        verify_registered_key(&conn, &id, &embedder.key()).expect("verify"),
        Ok(())
    );

    // Idempotent: the same six-field key converges on the first id.
    let for_tx = embedder.clone();
    let again = db
        .writer()
        .transaction(move |tx| {
            register_embedder_representation(
                tx,
                "33333333-3333-7333-8333-333333333333",
                for_tx.as_ref(),
                NOW_MS + 1,
            )
        })
        .await
        .expect("re-register");
    assert_eq!(again, REPRESENTATION_ID, "duplicate key must converge");
}

/// Each of the six canonical fields is compared, and the mismatch names it.
#[tokio::test(flavor = "multi_thread")]
async fn every_diverging_key_field_is_named() {
    let (_home, db) = open_state();
    let registered = HashingEmbedder::new(RepresentationKind::CodeRaw).key();

    let key = registered.clone();
    db.writer()
        .transaction(move |tx| {
            local_rag_store::register_representation(tx, REPRESENTATION_ID, &key, NOW_MS)
        })
        .await
        .expect("register representation");
    let conn = db.open_read().expect("read connection");

    let variants: Vec<(&'static str, RepresentationKey)> = vec![
        (
            "kind",
            RepresentationKey {
                kind: RepresentationKind::Memory,
                ..registered.clone()
            },
        ),
        (
            "representation_version",
            RepresentationKey {
                representation_version: registered.representation_version + 1,
                ..registered.clone()
            },
        ),
        (
            "normalization_version",
            RepresentationKey {
                normalization_version: registered.normalization_version + 1,
                ..registered.clone()
            },
        ),
        (
            "model_id",
            RepresentationKey {
                model_id: "some-other-model".to_string(),
                ..registered.clone()
            },
        ),
        (
            "dimensions",
            RepresentationKey {
                dimensions: registered.dimensions * 2,
                ..registered.clone()
            },
        ),
        (
            "distance_metric",
            RepresentationKey {
                distance_metric: DistanceMetric::L2,
                ..registered.clone()
            },
        ),
    ];

    for (field, candidate) in variants {
        let outcome = verify_registered_key(&conn, REPRESENTATION_ID, &candidate).expect("verify");
        match outcome {
            Err(RegistryMismatch::Field {
                field: reported, ..
            }) => assert_eq!(reported, field, "wrong field reported for {field}"),
            other => panic!("expected a {field} mismatch, got {other:?}"),
        }
    }
}

/// An unregistered id is distinguished from a field mismatch.
#[test]
fn an_unregistered_representation_is_its_own_mismatch() {
    let (_home, db) = open_state();
    let conn = db.open_read().expect("read connection");
    let key = HashingEmbedder::new(RepresentationKind::CodeRaw).key();

    let outcome = verify_registered_key(&conn, REPRESENTATION_ID, &key).expect("verify");
    assert_eq!(
        outcome,
        Err(RegistryMismatch::Unregistered {
            representation_id: REPRESENTATION_ID.to_string(),
        })
    );
}

/// Vectors that reach `embedding_cache` must match the registered
/// dimensionality — the cache's own `dimensions * 4 == byte_size` invariant
/// (spec 03 §4.2) depends on it.
#[tokio::test(flavor = "multi_thread")]
async fn vector_width_matches_the_registered_dimensions() {
    let (_home, db) = open_state();
    let embedder = Arc::new(HashingEmbedder::new(RepresentationKind::CodeRaw));

    let for_tx = embedder.clone();
    db.writer()
        .transaction(move |tx| {
            register_embedder_representation(tx, REPRESENTATION_ID, for_tx.as_ref(), NOW_MS)
        })
        .await
        .expect("register representation");
    let conn = db.open_read().expect("read connection");
    let registered = local_rag_store::representation_key(&conn, REPRESENTATION_ID)
        .expect("read key")
        .expect("registered");

    let pool = ProviderPool::new(vec![ProviderEntry::local("local", embedder)]);
    let vectors = pool
        .embed(
            DataPolicy::LocalOnly,
            EmbedRequest::new(RepresentationKind::CodeRaw, batch()),
        )
        .expect("embed");

    for vector in &vectors {
        assert_eq!(vector.dimensions(), registered.dimensions as usize);
        assert_eq!(
            encode_vector_le(vector.as_slice()).len(),
            registered.dimensions as usize * 4,
            "byte width must satisfy the cache invariant"
        );
    }
}

/// A provider that contradicts its own `key()` is rejected by the pool rather
/// than allowed to write a mis-sized vector.
#[test]
fn a_provider_contradicting_its_own_key_is_rejected() {
    /// Declares `dimensions = 256` but returns 8-component vectors.
    struct LyingEmbedder {
        key: RepresentationKey,
    }

    impl Embedder for LyingEmbedder {
        fn embed(&self, req: EmbedRequest) -> Result<Vec<Vector>, EmbedError> {
            Ok(req
                .texts
                .iter()
                .map(|_| Vector::new(vec![0.1; 8]))
                .collect())
        }

        fn key(&self) -> RepresentationKey {
            self.key.clone()
        }
    }

    let key = HashingEmbedder::new(RepresentationKind::CodeRaw).key();
    let pool = ProviderPool::new(vec![ProviderEntry::local(
        "liar",
        Arc::new(LyingEmbedder { key: key.clone() }),
    )]);

    let err = pool
        .embed(
            DataPolicy::LocalOnly,
            EmbedRequest::new(RepresentationKind::CodeRaw, batch()),
        )
        .expect_err("dimension contradiction must fail");

    match err {
        EmbedError::DimensionMismatch {
            provider,
            expected,
            actual,
            index,
        } => {
            assert_eq!(provider, "liar");
            assert_eq!(expected, key.dimensions);
            assert_eq!(actual, 8);
            assert_eq!(index, 0);
        }
        other => panic!("expected DimensionMismatch, got {other}"),
    }
}

/// A provider that drops or reorders rows breaks the positional contract and is
/// caught before its output can be keyed by the wrong subject.
#[test]
fn a_provider_returning_the_wrong_row_count_is_rejected() {
    /// Answers only the first text of the batch.
    struct Truncating;
    impl Embedder for Truncating {
        fn embed(&self, req: EmbedRequest) -> Result<Vec<Vector>, EmbedError> {
            let e = HashingEmbedder::new(req.kind);
            Ok(req.texts.iter().take(1).map(|t| e.embed_one(t)).collect())
        }
        fn key(&self) -> RepresentationKey {
            HashingEmbedder::new(RepresentationKind::CodeRaw).key()
        }
    }

    let pool = ProviderPool::new(vec![ProviderEntry::local(
        "truncating",
        Arc::new(Truncating),
    )]);
    let err = pool
        .embed(
            DataPolicy::LocalOnly,
            EmbedRequest::new(RepresentationKind::CodeRaw, batch()),
        )
        .expect_err("truncated batch must fail");

    match err {
        EmbedError::ResultCountMismatch {
            provider,
            expected,
            actual,
        } => {
            assert_eq!(provider, "truncating");
            assert_eq!(expected, batch().len());
            assert_eq!(actual, 1);
        }
        other => panic!("expected ResultCountMismatch, got {other}"),
    }
}
