//! Tying a provider to the representation registry (spec 03 §2.2, 10 §2).
//!
//! `embedding_cache` rows reference a `representation_id` and "never inline
//! model params" `[FIXED]` (spec 10 §2). That makes one question load-bearing
//! before a single vector is written: **is the provider that produced this
//! vector the provider that `representation_id` describes?** A drift here —
//! a provider swapped for one with different dimensions, metric or model — is
//! invisible in the cache rows themselves, because they carry only the id.
//!
//! So this module offers the two halves of that check:
//!
//! * [`register_embedder_representation`] — take the provider's own
//!   [`Embedder::key`] and register it, converging on the existing row when the
//!   six-field key is already known (`register_representation`'s `ON CONFLICT`
//!   idiom, T11-01);
//! * [`verify_registered_key`] — compare a stored row against a provider's key
//!   field by field, producing a typed [`RegistryMismatch`] that names the field
//!   that diverged, not a bare bool.

use std::fmt;

use local_rag_store::rusqlite::{Connection, Transaction};
use local_rag_store::{RepresentationKey, register_representation, representation_key, rusqlite};

use crate::contract::Embedder;

/// Why a provider does not match the representation row it was checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryMismatch {
    /// No `representation` row exists under that id.
    Unregistered {
        /// The id that was looked up.
        representation_id: String,
    },
    /// A field of the stored key differs from the provider's key.
    Field {
        /// The id that was looked up.
        representation_id: String,
        /// Which of the six canonical fields diverged.
        field: &'static str,
        /// What the registry holds.
        registered: String,
        /// What the provider declares.
        provider: String,
    },
}

impl fmt::Display for RegistryMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryMismatch::Unregistered { representation_id } => {
                write!(f, "representation {representation_id} is not registered")
            }
            RegistryMismatch::Field {
                representation_id,
                field,
                registered,
                provider,
            } => write!(
                f,
                "representation {representation_id} has {field}={registered}, provider declares {provider}"
            ),
        }
    }
}

impl std::error::Error for RegistryMismatch {}

/// Register the provider's own representation key, returning its
/// `representation_id`.
///
/// Idempotent: a key already registered converges on the first-registered id
/// (T11-01's `INSERT ... ON CONFLICT (the six fields) DO UPDATE ... RETURNING`),
/// so calling this on every daemon start is safe and never creates a second row.
pub fn register_embedder_representation(
    tx: &Transaction<'_>,
    representation_id: &str,
    embedder: &dyn Embedder,
    now_ms: i64,
) -> rusqlite::Result<String> {
    register_representation(tx, representation_id, &embedder.key(), now_ms)
}

/// Verify that the stored representation row equals `key` in all six canonical
/// fields (spec 03 §2.2's `UNIQUE` tuple).
///
/// The comparison is field-by-field on purpose: an equality check would answer
/// "no", while a caller diagnosing a stale model space needs to know *which*
/// field moved (dimensions and distance metric in particular decide whether a
/// shard can be reused at all — spec 10 §4 `[FIXED]`: different dimensions ⇒
/// separate shard layout, never in place).
pub fn verify_registered_key(
    conn: &Connection,
    representation_id: &str,
    key: &RepresentationKey,
) -> rusqlite::Result<Result<(), RegistryMismatch>> {
    let Some(stored) = representation_key(conn, representation_id)? else {
        return Ok(Err(RegistryMismatch::Unregistered {
            representation_id: representation_id.to_string(),
        }));
    };

    let mismatch = |field: &'static str, registered: String, provider: String| {
        Err(RegistryMismatch::Field {
            representation_id: representation_id.to_string(),
            field,
            registered,
            provider,
        })
    };

    if stored.kind != key.kind {
        return Ok(mismatch(
            "kind",
            stored.kind.as_str().to_string(),
            key.kind.as_str().to_string(),
        ));
    }
    if stored.representation_version != key.representation_version {
        return Ok(mismatch(
            "representation_version",
            stored.representation_version.to_string(),
            key.representation_version.to_string(),
        ));
    }
    if stored.normalization_version != key.normalization_version {
        return Ok(mismatch(
            "normalization_version",
            stored.normalization_version.to_string(),
            key.normalization_version.to_string(),
        ));
    }
    if stored.model_id != key.model_id {
        return Ok(mismatch(
            "model_id",
            stored.model_id.clone(),
            key.model_id.clone(),
        ));
    }
    if stored.dimensions != key.dimensions {
        return Ok(mismatch(
            "dimensions",
            stored.dimensions.to_string(),
            key.dimensions.to_string(),
        ));
    }
    if stored.distance_metric != key.distance_metric {
        return Ok(mismatch(
            "distance_metric",
            stored.distance_metric.as_str().to_string(),
            key.distance_metric.as_str().to_string(),
        ));
    }
    Ok(Ok(()))
}
